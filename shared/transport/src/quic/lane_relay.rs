//! Lane relay for a Pier whose session runs in another process (ADR 0014).
//!
//! The Windows Pier's broker and the macOS Pier's service own the network
//! connection; the session itself (capture, encode, injection, audio) runs in
//! a desktop agent behind one local message socket. [`PierLaneRelay`] is the
//! network half of that split: it takes every session message the agent
//! produces and puts it on its own lane, and turns every lane the Deck opens
//! back into session messages for the agent.
//!
//! - Video and audio frames go to their own writer tasks. Routing never
//!   waits: a full video backlog drops the frame, holds that monitor's
//!   video until its next keyframe and asks the agent for one
//!   (`request_full_frame`); a full audio backlog drops the frame. Neither
//!   ever falls back onto another lane, so a blocked video lane cannot hold
//!   audio, input or control behind it.
//! - Cursor messages go on the input/cursor lane, newest first: a shape the
//!   lane has no room for is replaced by the next one, never queued.
//! - Clipboard, HID and USB messages go on the bulk lane through a bounded
//!   backlog; a full backlog (or a bulk lane that stays unavailable) drops
//!   and counts the message. A dropped clipboard transfer is abandoned
//!   visibly: the rest of it is dropped too and the Deck gets
//!   `clipboard_transfer_failed` on the control lane.
//! - Control messages are handed back to the caller, which sends them on the
//!   control lane exactly as before ([`RelayRoute::Control`]).
//! - Input from the Deck is forwarded in order and never dropped; the input
//!   lane's own back-pressure (and the Deck's motion coalescing) bounds it.
//!   A superseded, failed or ended input lane is followed at once by
//!   `key_reset_modifiers` (reason [`RELAY_INPUT_RELEASE_REASON`], a full
//!   release) on the same ordered channel, so the agent releases held input
//!   before anything a replacement lane carries.
//! - Microphone frames are forwarded with a bounded backlog; the agent keeps
//!   its own consent and generation, which no lane recovery touches.
//!
//! Everything the relay needs from the caller arrives through
//! [`PierLaneRelay::route_to_deck`] and [`PierLaneRelay::next_event`]; both
//! are cancel-safe, so the caller keeps one `select!` loop over its agent
//! socket, its control lane and the relay.

use std::collections::{HashSet, VecDeque};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arcen_protocol::lanes::{
    LaneRole, SessionRecord, clipboard_transfer_sequence, lane_for_binary_message,
    lane_for_session_text, session_text_record,
};
use arcen_protocol::messages::{
    AudioStreamReason, AudioStreamResultMsg, ClipboardTransferFailedMsg, ClipboardTransferFailure,
    KeyResetModifiersMsg, MicrophoneStreamReason, MicrophoneStreamStopMsg, RequestFullFrameMsg,
};
use arcen_protocol::wire::{FrameType, decode_microphone_frame, decode_video_header};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::session_lanes::{
    LaneSenderSlot, LaneTerminal, LaneWriteOutcome, LocalLaneWriter, PeerLaneEvent, PeerLaneReader,
    session_record_bytes,
};
use super::{LaneConnection, LaneSendError};
use crate::lanes::LaneFailure;

/// Video frames waiting for the video lane beyond the lane's own queue.
/// With the lane queue this is about a third of a second at 60 fps; past
/// it the relay drops and resynchronises rather than stalling the agent.
pub const RELAY_VIDEO_BACKLOG: usize = 16;
/// Audio frames waiting for the audio lane writer.
pub const RELAY_AUDIO_BACKLOG: usize = 8;
/// Bulk messages waiting for the bulk lane.
pub const RELAY_BULK_BACKLOG: usize = 64;
/// Deck input records waiting for the agent.
pub const RELAY_INPUT_BACKLOG: usize = 256;
/// Deck microphone frames waiting for the agent.
pub const RELAY_MICROPHONE_BACKLOG: usize = 16;
/// Deck bulk records waiting for the agent.
pub const RELAY_BULK_INBOUND_BACKLOG: usize = 64;
/// How long one bulk message may wait for the Deck's bulk lane.
pub const RELAY_BULK_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Minimum spacing of keyframe requests while video resynchronises.
const KEYFRAME_REQUEST_SPACING: Duration = Duration::from_millis(250);
/// How often a cursor shape waiting for input-lane room is retried.
const CURSOR_RETRY: Duration = Duration::from_millis(20);
/// Reason carried by the relay's held-input release: release everything.
pub const RELAY_INPUT_RELEASE_REASON: &str =
    arcen_protocol::messages::RELEASE_ALL_HELD_INPUT_REASON;

/// Where [`PierLaneRelay::route_to_deck`] put a session message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayRoute {
    /// A control-lane message: the caller sends it on the control lane.
    Control(SessionRecord),
    /// Queued for the named lane's writer.
    Queued(LaneRole),
    /// Dropped under the named lane's policy (full backlog, video waiting
    /// for a keyframe, disabled role, or a message no lane accepts from the
    /// Pier). Counted in [`LaneRelayStats`].
    Dropped(LaneRole),
}

/// What the relay asks of its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneRelayEvent {
    /// Deliver to the agent, in order: Deck input, microphone and bulk
    /// records, and the relay's own `request_full_frame` and
    /// `key_reset_modifiers`.
    ToSession(SessionRecord),
    /// Send to the Deck on the control lane (audio disabled visibly,
    /// microphone stopped).
    ToDeckControl(SessionRecord),
    /// The attachment cannot continue on lanes (video or input exhausted
    /// its recovery, the attachment ended, the connection was lost). The
    /// caller ends the attachment into its reconnect policy.
    Ended(LaneTerminal),
}

/// Counters of one relay, for health snapshots and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaneRelayStats {
    /// Video frames queued for the video lane.
    pub video_frames: u64,
    /// Video frames dropped (full backlog or waiting for a keyframe).
    pub video_dropped: u64,
    /// `request_full_frame` messages the relay sent to the agent.
    pub keyframe_requests: u64,
    /// Audio frames queued on the audio lane.
    pub audio_frames: u64,
    /// Audio frames dropped (full backlog or lane queue, disabled lane).
    pub audio_dropped: u64,
    /// Cursor shapes replaced by a newer one before the lane had room.
    pub cursor_replaced: u64,
    /// Bulk messages dropped (full backlog, lane unavailable).
    pub bulk_dropped: u64,
    /// Clipboard transfers abandoned (the Deck was told).
    pub clipboard_failed: u64,
    /// Deck input records forwarded to the agent.
    pub input_records: u64,
    /// Deck microphone frames forwarded to the agent.
    pub microphone_frames: u64,
    /// Deck microphone frames dropped (full backlog).
    pub microphone_dropped: u64,
}

#[derive(Default)]
struct Counters {
    video_frames: AtomicU64,
    video_dropped: AtomicU64,
    keyframe_requests: AtomicU64,
    audio_frames: AtomicU64,
    audio_dropped: AtomicU64,
    cursor_replaced: AtomicU64,
    bulk_dropped: AtomicU64,
    clipboard_failed: AtomicU64,
    input_records: AtomicU64,
    microphone_frames: AtomicU64,
    microphone_dropped: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    fn snapshot(&self) -> LaneRelayStats {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        LaneRelayStats {
            video_frames: read(&self.video_frames),
            video_dropped: read(&self.video_dropped),
            keyframe_requests: read(&self.keyframe_requests),
            audio_frames: read(&self.audio_frames),
            audio_dropped: read(&self.audio_dropped),
            cursor_replaced: read(&self.cursor_replaced),
            bulk_dropped: read(&self.bulk_dropped),
            clipboard_failed: read(&self.clipboard_failed),
            input_records: read(&self.input_records),
            microphone_frames: read(&self.microphone_frames),
            microphone_dropped: read(&self.microphone_dropped),
        }
    }
}

/// Per-monitor prediction chains the relay broke by dropping a frame.
/// A monitor in the set passes no frame until its next keyframe.
#[derive(Debug, Default)]
struct VideoResync {
    seen: HashSet<u16>,
    waiting: HashSet<u16>,
    last_request: Option<Instant>,
}

impl VideoResync {
    /// Whether a frame of `monitor` may be queued; a keyframe ends the wait.
    fn admits(&mut self, monitor: u16, keyframe: bool) -> bool {
        self.seen.insert(monitor);
        if keyframe {
            self.waiting.remove(&monitor);
            return true;
        }
        !self.waiting.contains(&monitor)
    }

    /// A frame was dropped: every chain this relay carries is broken, since
    /// the agent's keyframe request is not per monitor. Returns whether a
    /// new keyframe request is due.
    fn broke(&mut self, monitor: u16, now: Instant) -> bool {
        self.seen.insert(monitor);
        self.waiting.extend(self.seen.iter().copied());
        self.request_due(now)
    }

    /// Whether a keyframe request is due now; records it when it is.
    fn request_due(&mut self, now: Instant) -> bool {
        let due = self
            .last_request
            .is_none_or(|last| now.duration_since(last) >= KEYFRAME_REQUEST_SPACING);
        if due {
            self.last_request = Some(now);
        }
        due
    }

    /// A replacement lane starts every chain again.
    fn restart(&mut self) {
        self.waiting.extend(self.seen.iter().copied());
        self.last_request = Some(Instant::now());
    }

    #[cfg(test)]
    fn is_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }
}

fn keyframe_request() -> Option<SessionRecord> {
    session_text_record(&RequestFullFrameMsg::default())
}

fn input_release() -> Option<SessionRecord> {
    session_text_record(&KeyResetModifiersMsg {
        reason: RELAY_INPUT_RELEASE_REASON.to_owned(),
        ..KeyResetModifiersMsg::default()
    })
}

/// The network half of a Pier whose session runs behind a local socket.
/// Dropping it aborts every lane task it started.
pub struct PierLaneRelay {
    video: mpsc::Sender<Bytes>,
    audio: mpsc::Sender<Bytes>,
    bulk: mpsc::Sender<BulkRecord>,
    failed_clipboard: Option<u64>,
    input_slot: LaneSenderSlot,
    pending_cursor: Option<Bytes>,
    cursor_retry: tokio::time::Interval,
    resync: VideoResync,
    owed: VecDeque<LaneRelayEvent>,
    events: mpsc::Receiver<TaskEvent>,
    input: mpsc::Receiver<SessionRecord>,
    microphone: mpsc::Receiver<SessionRecord>,
    bulk_in: mpsc::Receiver<SessionRecord>,
    counters: Arc<Counters>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for PierLaneRelay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PierLaneRelay")
            .field("stats", &self.counters.snapshot())
            .finish_non_exhaustive()
    }
}

impl Drop for PierLaneRelay {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl PierLaneRelay {
    /// Starts the relay's lane tasks. Call it after the attachment was
    /// granted ([`super::grant_attachment`]) and its `lane_attachment` sent:
    /// the video and audio lanes open as soon as their first frame arrives.
    #[must_use]
    pub fn start(lanes: &LaneConnection) -> Self {
        let counters = Arc::new(Counters::default());
        let (video_tx, video_rx) = mpsc::channel(RELAY_VIDEO_BACKLOG);
        let (audio_tx, audio_rx) = mpsc::channel(RELAY_AUDIO_BACKLOG);
        let (bulk_tx, bulk_rx) = mpsc::channel(RELAY_BULK_BACKLOG);
        let (events_tx, events_rx) = mpsc::channel(16);
        let (input_tx, input_rx) = mpsc::channel(RELAY_INPUT_BACKLOG);
        let (microphone_tx, microphone_rx) = mpsc::channel(RELAY_MICROPHONE_BACKLOG);
        let (bulk_in_tx, bulk_in_rx) = mpsc::channel(RELAY_BULK_INBOUND_BACKLOG);
        let input_slot = LaneSenderSlot::new(LaneRole::InputCursor);
        let bulk_slot = LaneSenderSlot::new(LaneRole::BulkAux);
        let tasks = vec![
            tokio::spawn(video_writer(lanes.clone(), video_rx, events_tx.clone())),
            tokio::spawn(audio_writer(
                lanes.clone(),
                audio_rx,
                events_tx.clone(),
                Arc::clone(&counters),
            )),
            tokio::spawn(bulk_writer(
                bulk_rx,
                bulk_slot.clone(),
                events_tx.clone(),
                Arc::clone(&counters),
            )),
            tokio::spawn(input_reader(
                lanes.clone(),
                input_slot.clone(),
                input_tx,
                events_tx.clone(),
                Arc::clone(&counters),
            )),
            tokio::spawn(microphone_reader(
                lanes.clone(),
                microphone_tx,
                events_tx.clone(),
                Arc::clone(&counters),
            )),
            tokio::spawn(bulk_reader(lanes.clone(), bulk_slot, bulk_in_tx, events_tx)),
        ];
        let mut cursor_retry = tokio::time::interval(CURSOR_RETRY);
        cursor_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            video: video_tx,
            audio: audio_tx,
            bulk: bulk_tx,
            failed_clipboard: None,
            input_slot,
            pending_cursor: None,
            cursor_retry,
            resync: VideoResync::default(),
            owed: VecDeque::new(),
            events: events_rx,
            input: input_rx,
            microphone: microphone_rx,
            bulk_in: bulk_in_rx,
            counters,
            tasks,
        }
    }

    /// Counters so far.
    #[must_use]
    pub fn stats(&self) -> LaneRelayStats {
        self.counters.snapshot()
    }

    /// Puts one message from the agent on its lane, without waiting.
    ///
    /// Control messages (and ping, pong and close) come back as
    /// [`RelayRoute::Control`] for the caller's control-lane socket.
    pub fn route_to_deck(&mut self, record: SessionRecord) -> RelayRoute {
        match record {
            SessionRecord::Text(text) => match lane_for_session_text(&text) {
                LaneRole::InputCursor => self.route_cursor(text),
                LaneRole::BulkAux => self.route_bulk(&SessionRecord::Text(text)),
                _ => RelayRoute::Control(SessionRecord::Text(text)),
            },
            SessionRecord::Binary(bytes) => match lane_for_binary_message(&bytes) {
                Some(LaneRole::Video) => self.route_video(bytes),
                Some(LaneRole::AudioOutput) => self.route_audio(bytes),
                Some(LaneRole::BulkAux) => self.route_bulk(&SessionRecord::Binary(bytes)),
                // Microphone frames flow Deck to Pier only.
                Some(LaneRole::Microphone) => RelayRoute::Dropped(LaneRole::Microphone),
                _ => RelayRoute::Control(SessionRecord::Binary(bytes)),
            },
            other => RelayRoute::Control(other),
        }
    }

    fn route_video(&mut self, frame: Vec<u8>) -> RelayRoute {
        let Ok(header) = decode_video_header(&frame) else {
            bump(&self.counters.video_dropped);
            return RelayRoute::Dropped(LaneRole::Video);
        };
        let keyframe = header.is_keyframe();
        if !self.resync.admits(header.monitor_id, keyframe) {
            bump(&self.counters.video_dropped);
            self.request_keyframe_if_due();
            return RelayRoute::Dropped(LaneRole::Video);
        }
        if self.video.try_send(Bytes::from(frame)).is_ok() {
            bump(&self.counters.video_frames);
            RelayRoute::Queued(LaneRole::Video)
        } else {
            bump(&self.counters.video_dropped);
            self.break_video_chain(header.monitor_id);
            RelayRoute::Dropped(LaneRole::Video)
        }
    }

    fn break_video_chain(&mut self, monitor: u16) {
        if self.resync.broke(monitor, Instant::now()) {
            self.owe_keyframe_request();
        }
    }

    /// A frame was held back while its monitor waits: ask again if the
    /// last request is old enough (the agent may have missed it).
    fn request_keyframe_if_due(&mut self) {
        if self.resync.request_due(Instant::now()) {
            self.owe_keyframe_request();
        }
    }

    fn owe_keyframe_request(&mut self) {
        if let Some(request) = keyframe_request() {
            bump(&self.counters.keyframe_requests);
            self.owed.push_back(LaneRelayEvent::ToSession(request));
        }
    }

    fn route_audio(&mut self, frame: Vec<u8>) -> RelayRoute {
        if self.audio.try_send(Bytes::from(frame)).is_ok() {
            RelayRoute::Queued(LaneRole::AudioOutput)
        } else {
            bump(&self.counters.audio_dropped);
            RelayRoute::Dropped(LaneRole::AudioOutput)
        }
    }

    fn route_cursor(&mut self, text: String) -> RelayRoute {
        if self.pending_cursor.is_some() {
            bump(&self.counters.cursor_replaced);
        }
        self.pending_cursor = Some(session_record_bytes(&SessionRecord::Text(text)));
        self.flush_cursor();
        RelayRoute::Queued(LaneRole::InputCursor)
    }

    fn flush_cursor(&mut self) {
        let (Some(record), Some(sender)) = (self.pending_cursor.take(), self.input_slot.current())
        else {
            return;
        };
        if let Err(error) = sender.try_send(record) {
            self.pending_cursor = error.into_record();
        }
    }

    fn route_bulk(&mut self, record: &SessionRecord) -> RelayRoute {
        let clipboard = clipboard_transfer_sequence(record);
        if clipboard.is_some() && clipboard == self.failed_clipboard {
            // The rest of an abandoned transfer.
            bump(&self.counters.bulk_dropped);
            return RelayRoute::Dropped(LaneRole::BulkAux);
        }
        let queued = self.bulk.try_send(BulkRecord {
            bytes: session_record_bytes(record),
            clipboard,
        });
        if queued.is_ok() {
            return RelayRoute::Queued(LaneRole::BulkAux);
        }
        bump(&self.counters.bulk_dropped);
        if let Some(sequence) = clipboard {
            self.clipboard_failed(sequence, ClipboardTransferFailure::BulkLaneFull);
        }
        RelayRoute::Dropped(LaneRole::BulkAux)
    }

    /// Abandons a clipboard transfer and tells the Deck, once per transfer.
    fn clipboard_failed(&mut self, sequence: u64, reason: ClipboardTransferFailure) {
        if self.failed_clipboard == Some(sequence) {
            return;
        }
        self.failed_clipboard = Some(sequence);
        bump(&self.counters.clipboard_failed);
        if let Some(notice) =
            session_text_record(&ClipboardTransferFailedMsg::new(sequence, reason))
        {
            self.owed.push_back(LaneRelayEvent::ToDeckControl(notice));
        }
    }

    /// The next thing the caller must do. Cancel-safe.
    ///
    /// Input is served first, then the relay's own events, microphone and
    /// bulk; none waits for another.
    pub async fn next_event(&mut self) -> LaneRelayEvent {
        loop {
            if let Some(owed) = self.owed.pop_front() {
                return owed;
            }
            tokio::select! {
                biased;
                Some(record) = self.input.recv() => return LaneRelayEvent::ToSession(record),
                Some(event) = self.events.recv() => {
                    if let Some(event) = self.apply(event) {
                        return event;
                    }
                }
                Some(record) = self.microphone.recv() => return LaneRelayEvent::ToSession(record),
                Some(record) = self.bulk_in.recv() => return LaneRelayEvent::ToSession(record),
                _ = self.cursor_retry.tick(), if self.pending_cursor.is_some() => {
                    self.flush_cursor();
                }
            }
        }
    }

    /// Applies a task event; returns what the caller must see of it.
    fn apply(&mut self, event: TaskEvent) -> Option<LaneRelayEvent> {
        match event {
            TaskEvent::Relay(event) => Some(event),
            TaskEvent::VideoReopened => {
                self.resync.restart();
                self.owe_keyframe_request();
                None
            }
            TaskEvent::ClipboardUndelivered(sequence) => {
                self.clipboard_failed(sequence, ClipboardTransferFailure::BulkLaneUnavailable);
                None
            }
        }
    }
}

/// What a lane task tells the relay.
#[derive(Debug)]
enum TaskEvent {
    Relay(LaneRelayEvent),
    /// The video lane was replaced: every chain starts again.
    VideoReopened,
    /// The bulk writer could not deliver part of this clipboard transfer.
    ClipboardUndelivered(u64),
}

/// One bulk record and the clipboard transfer it belongs to, if any.
#[derive(Debug)]
struct BulkRecord {
    bytes: Bytes,
    clipboard: Option<u64>,
}

async fn report(events: &mpsc::Sender<TaskEvent>, event: LaneRelayEvent) {
    let _ = events.send(TaskEvent::Relay(event)).await;
}

async fn video_writer(
    lanes: LaneConnection,
    mut frames: mpsc::Receiver<Bytes>,
    events: mpsc::Sender<TaskEvent>,
) {
    let mut writer: Option<LocalLaneWriter> = None;
    while let Some(frame) = frames.recv().await {
        if writer.is_none() {
            match LocalLaneWriter::open(&lanes, LaneRole::Video).await {
                Ok(opened) => writer = Some(opened),
                Err(terminal) => return report(&events, LaneRelayEvent::Ended(terminal)).await,
            }
        }
        let Some(lane) = writer.as_mut() else {
            continue;
        };
        match lane.send(frame).await {
            Ok(LaneWriteOutcome::Reopened { request_keyframe }) => {
                if request_keyframe {
                    let _ = events.send(TaskEvent::VideoReopened).await;
                }
            }
            Ok(_) => {}
            Err(terminal) => return report(&events, LaneRelayEvent::Ended(terminal)).await,
        }
    }
    if let Some(writer) = writer {
        writer.finish();
    }
}

async fn audio_writer(
    lanes: LaneConnection,
    mut frames: mpsc::Receiver<Bytes>,
    events: mpsc::Sender<TaskEvent>,
    counters: Arc<Counters>,
) {
    let mut writer: Option<LocalLaneWriter> = None;
    let mut disabled = false;
    while let Some(frame) = frames.recv().await {
        if disabled {
            bump(&counters.audio_dropped);
            continue;
        }
        if writer.is_none() {
            match LocalLaneWriter::open(&lanes, LaneRole::AudioOutput).await {
                Ok(opened) => writer = Some(opened),
                Err(terminal) => {
                    disabled = audio_stopped(terminal, &events).await;
                    if !disabled {
                        return;
                    }
                    continue;
                }
            }
        }
        let Some(lane) = writer.as_mut() else {
            continue;
        };
        match lane.try_send(frame).await {
            Ok(LaneWriteOutcome::Queued | LaneWriteOutcome::Reopened { .. }) => {
                bump(&counters.audio_frames);
            }
            Ok(LaneWriteOutcome::DroppedFull) => bump(&counters.audio_dropped),
            Err(terminal) => {
                writer = None;
                disabled = audio_stopped(terminal, &events).await;
                if !disabled {
                    return;
                }
            }
        }
    }
    if let Some(writer) = writer {
        writer.finish();
    }
}

/// Reports an audio lane that stopped. A disabled lane disables audio
/// visibly and keeps the attachment; returns whether that was the case.
async fn audio_stopped(terminal: LaneTerminal, events: &mpsc::Sender<TaskEvent>) -> bool {
    match terminal {
        LaneTerminal::Disabled(_) => {
            if let Some(result) = session_text_record(&AudioStreamResultMsg::disabled(
                AudioStreamReason::LaneUnavailable,
            )) {
                report(events, LaneRelayEvent::ToDeckControl(result)).await;
            }
            true
        }
        other => {
            report(events, LaneRelayEvent::Ended(other)).await;
            false
        }
    }
}

async fn bulk_writer(
    mut records: mpsc::Receiver<BulkRecord>,
    slot: LaneSenderSlot,
    events: mpsc::Sender<TaskEvent>,
    counters: Arc<Counters>,
) {
    let mut failed: Option<u64> = None;
    while let Some(record) = records.recv().await {
        if record.clipboard.is_some() && record.clipboard == failed {
            bump(&counters.bulk_dropped);
            continue;
        }
        if send_on_slot(&slot, record.bytes, RELAY_BULK_WRITE_TIMEOUT).await {
            continue;
        }
        bump(&counters.bulk_dropped);
        if let Some(sequence) = record.clipboard {
            failed = Some(sequence);
            let _ = events.send(TaskEvent::ClipboardUndelivered(sequence)).await;
        }
    }
}

/// Queues one record on the newest lane in `slot`, waiting for a lane and
/// for room in its queue up to `timeout`. Returns whether it was queued.
async fn send_on_slot(slot: &LaneSenderSlot, record: Bytes, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            let sender = slot.wait().await;
            match sender.send(record.clone()).await {
                Ok(()) => return true,
                Err(LaneSendError::Closed(_)) => {
                    // The reader installs the replacement; drop the stale one.
                    tokio::task::yield_now().await;
                    if slot.current().is_some_and(|current| {
                        current.lane_generation() == sender.lane_generation()
                    }) {
                        slot.clear();
                    }
                }
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Deck input to the agent, in order and never dropped; held input is
/// released whenever the lane is replaced, fails or ends.
async fn input_reader(
    lanes: LaneConnection,
    slot: LaneSenderSlot,
    input: mpsc::Sender<SessionRecord>,
    events: mpsc::Sender<TaskEvent>,
    counters: Arc<Counters>,
) {
    let mut reader = PeerLaneReader::new(&lanes, LaneRole::InputCursor);
    // The release travels on the same ordered channel as the input itself,
    // so it always reaches the agent before anything the replacement lane
    // carries.
    let release = || async {
        match input_release() {
            Some(record) => input.send(record).await.is_ok(),
            None => true,
        }
    };
    loop {
        match reader.next().await {
            Ok(PeerLaneEvent::Opened {
                release_held_input,
                sender,
                ..
            }) => {
                if release_held_input && !release().await {
                    return;
                }
                if let Some(sender) = sender {
                    slot.install(sender);
                }
            }
            Ok(PeerLaneEvent::Lost {
                release_held_input, ..
            }) => {
                slot.clear();
                if release_held_input && !release().await {
                    return;
                }
            }
            Ok(PeerLaneEvent::Frame(body)) => match SessionRecord::decode_body(&body) {
                Ok(SessionRecord::Text(text))
                    if lane_for_session_text(&text) == LaneRole::InputCursor =>
                {
                    bump(&counters.input_records);
                    if input.send(SessionRecord::Text(text)).await.is_err() {
                        return;
                    }
                }
                Ok(SessionRecord::Ping(_) | SessionRecord::Pong(_)) => {}
                Ok(_) | Err(_) => reader.fail(LaneFailure::MalformedFrame),
            },
            Err(terminal) => {
                slot.clear();
                if !release().await {
                    return;
                }
                if terminal != LaneTerminal::Finished {
                    report(&events, LaneRelayEvent::Ended(terminal)).await;
                }
                return;
            }
        }
    }
}

/// Deck microphone frames to the agent. A full backlog drops the frame (the
/// agent's jitter buffer conceals it); a disabled lane stops capture
/// visibly with the generation the frames carried.
async fn microphone_reader(
    lanes: LaneConnection,
    microphone: mpsc::Sender<SessionRecord>,
    events: mpsc::Sender<TaskEvent>,
    counters: Arc<Counters>,
) {
    let mut reader = PeerLaneReader::new(&lanes, LaneRole::Microphone);
    let mut generation: Option<NonZeroU32> = None;
    loop {
        match reader.next().await {
            Ok(PeerLaneEvent::Opened { .. } | PeerLaneEvent::Lost { .. }) => {}
            Ok(PeerLaneEvent::Frame(frame)) => {
                if frame.first().copied() != Some(FrameType::AudioUpstream as u8) {
                    reader.fail(LaneFailure::MalformedFrame);
                    continue;
                }
                if let Ok((header, _)) = decode_microphone_frame(&frame) {
                    generation = NonZeroU32::new(header.generation).or(generation);
                }
                match microphone.try_send(SessionRecord::Binary(frame.to_vec())) {
                    Ok(()) => bump(&counters.microphone_frames),
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        bump(&counters.microphone_dropped);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
            Err(LaneTerminal::Disabled(_)) => {
                if let Some(stop) = generation.and_then(|generation| {
                    session_text_record(&MicrophoneStreamStopMsg::new(
                        generation.get(),
                        MicrophoneStreamReason::CaptureFailure,
                    ))
                }) {
                    report(&events, LaneRelayEvent::ToDeckControl(stop)).await;
                }
                return;
            }
            Err(LaneTerminal::Finished) => return,
            Err(terminal) => return report(&events, LaneRelayEvent::Ended(terminal)).await,
        }
    }
}

/// Deck clipboard, HID and USB records to the agent; installs each bulk
/// lane's write half for [`bulk_writer`]. A disabled bulk lane turns those
/// features off for the attachment and nothing else.
async fn bulk_reader(
    lanes: LaneConnection,
    slot: LaneSenderSlot,
    bulk_in: mpsc::Sender<SessionRecord>,
    events: mpsc::Sender<TaskEvent>,
) {
    let mut reader = PeerLaneReader::new(&lanes, LaneRole::BulkAux);
    loop {
        match reader.next().await {
            Ok(PeerLaneEvent::Opened { sender, .. }) => {
                if let Some(sender) = sender {
                    slot.install(sender);
                }
            }
            Ok(PeerLaneEvent::Lost { .. }) => slot.clear(),
            Ok(PeerLaneEvent::Frame(body)) => match SessionRecord::decode_body(&body) {
                Ok(record) if is_bulk_record(&record) => {
                    if bulk_in.send(record).await.is_err() {
                        return;
                    }
                }
                Ok(SessionRecord::Ping(_) | SessionRecord::Pong(_)) => {}
                Ok(_) | Err(_) => reader.fail(LaneFailure::MalformedFrame),
            },
            Err(terminal) => {
                slot.clear();
                match terminal {
                    LaneTerminal::Disabled(_) | LaneTerminal::Finished => {}
                    other => report(&events, LaneRelayEvent::Ended(other)).await,
                }
                return;
            }
        }
    }
}

fn is_bulk_record(record: &SessionRecord) -> bool {
    match record {
        SessionRecord::Text(text) => lane_for_session_text(text) == LaneRole::BulkAux,
        SessionRecord::Binary(bytes) => lane_for_binary_message(bytes) == Some(LaneRole::BulkAux),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::VideoResync;
    use std::time::{Duration, Instant};

    #[test]
    fn a_dropped_frame_holds_every_known_monitor_until_its_keyframe() {
        let mut resync = VideoResync::default();
        let start = Instant::now();
        assert!(resync.admits(1, true));
        assert!(resync.admits(2, false));
        assert!(resync.broke(2, start));
        assert!(!resync.admits(1, false));
        assert!(!resync.admits(2, false));
        assert!(resync.admits(2, true));
        assert!(resync.admits(2, false));
        assert!(!resync.admits(1, false));
        assert!(resync.admits(1, true));
        assert!(!resync.is_waiting());
    }

    #[test]
    fn keyframe_requests_are_spaced() {
        let mut resync = VideoResync::default();
        let start = Instant::now();
        assert!(resync.broke(1, start));
        assert!(!resync.broke(1, start + Duration::from_millis(10)));
        assert!(resync.broke(1, start + Duration::from_millis(300)));
    }
}
