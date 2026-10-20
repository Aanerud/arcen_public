//! Per-lane workers of one Linux Pier attachment (ADR 0014).
//!
//! Each role has its own task: the video writer drains the monitor mux, the
//! audio writer drains the audio queue, the control writer serves control
//! messages and resume-grant refreshes, the bulk writer serves clipboard
//! payloads, and the input, microphone and bulk readers follow the lanes the
//! Deck opens. No worker awaits another role's lane: a blocked video lane
//! delays only video, and audio never falls back onto another lane.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arcen_protocol::lanes::{lane_for_session_text, LaneRole, SessionRecord};
use arcen_protocol::messages::{
    AudioStreamReason, ClipboardTransferFailedMsg, ClipboardTransferFailure,
    CLIPBOARD_TRANSFER_FAILED,
};
use arcen_telemetry::CorrelationId;
use arcen_transport::lanes::LaneFailure;
use arcen_transport::quic::{
    LaneConnection, LaneSenderSlot, LaneTerminal, LaneWriteOutcome, LocalLaneWriter, PeerLaneEvent,
    PeerLaneReader,
};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, trace, warn};

use super::{
    lock_recover, media, request_full_frame_idr, resume_refresh_interval, send_microphone_failure,
    send_successor_auth_result_or_drain, send_ws_with_timeout, AudioControl, AudioQueue,
    ClipboardWriterQueue, ControlSender, IdrRequester, MicrophoneFrameDecision,
    MicrophoneIngestOutcome, MicrophoneIngressRejection, MicrophoneIngressValidator,
    MicrophoneStatsTracker, PierInput, ResumeRefreshContext, SessionEndReason, VideoSource,
    WriterControl, AUDIO, INPUT, NET, SESSION,
};
use crate::session::resume::{message_bytes, record_to_message};

/// Aborts the tasks it holds when dropped (including when its owner is
/// aborted), so no lane worker outlives its attachment.
struct AbortOnDrop(Vec<JoinHandle<SessionEndReason>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Everything the attachment's writers own.
pub(super) struct LaneWriters<S> {
    pub(super) lanes: LaneConnection,
    pub(super) control_sink: S,
    pub(super) control: mpsc::Receiver<WriterControl>,
    pub(super) refresh: Option<ResumeRefreshContext>,
    pub(super) write_timeout: Duration,
    pub(super) video: VideoSource,
    pub(super) video_write_wait_micros: Arc<AtomicU64>,
    pub(super) idrs: Vec<IdrRequester>,
    pub(super) audio: Arc<AudioQueue>,
    pub(super) audio_control: AudioControl,
    pub(super) audio_failure: mpsc::Sender<AudioStreamReason>,
    pub(super) clipboard: Arc<ClipboardWriterQueue>,
    /// Where the bulk writer reports a clipboard transfer it had to abandon.
    pub(super) clipboard_notices: ControlSender,
    pub(super) input_slot: LaneSenderSlot,
    pub(super) bulk_slot: LaneSenderSlot,
}

/// Spawns one task per writing role and returns a supervisor that resolves
/// with the first reason a writer ended the attachment. Aborting the
/// supervisor aborts every writer.
pub(super) fn spawn_lane_writers<S>(writers: LaneWriters<S>) -> JoinHandle<SessionEndReason>
where
    S: futures_util::Sink<Message> + Unpin + Send + 'static,
    S::Error: std::fmt::Display,
{
    let LaneWriters {
        lanes,
        control_sink,
        control,
        refresh,
        write_timeout,
        video,
        video_write_wait_micros,
        idrs,
        audio,
        audio_control,
        audio_failure,
        clipboard,
        clipboard_notices,
        input_slot,
        bulk_slot,
    } = writers;
    let span = tracing::Span::current();
    let tasks = vec![
        tokio::spawn(tracing::Instrument::instrument(
            control_lane_writer(
                control_sink,
                control,
                refresh,
                write_timeout,
                input_slot,
                bulk_slot.clone(),
            ),
            span.clone(),
        )),
        tokio::spawn(tracing::Instrument::instrument(
            video_lane_writer(
                lanes.clone(),
                video,
                write_timeout,
                video_write_wait_micros,
                idrs,
            ),
            span.clone(),
        )),
        tokio::spawn(tracing::Instrument::instrument(
            audio_lane_writer(lanes, audio, audio_control, audio_failure),
            span.clone(),
        )),
        tokio::spawn(tracing::Instrument::instrument(
            bulk_lane_writer(clipboard, clipboard_notices, bulk_slot, write_timeout),
            span,
        )),
    ];
    tokio::spawn(async move {
        let mut guard = AbortOnDrop(tasks);
        let (result, _, _) = futures_util::future::select_all(guard.0.iter_mut()).await;
        drop(guard);
        result.unwrap_or(SessionEndReason::WriterEnded)
    })
}

/// Control messages and resume-grant refreshes, on the control lane only.
/// A message that belongs on another lane is handed to that lane without
/// waiting; it is never written to the control lane.
pub(super) async fn control_lane_writer<S>(
    mut sink: S,
    mut control: mpsc::Receiver<WriterControl>,
    refresh: Option<ResumeRefreshContext>,
    write_timeout: Duration,
    input_slot: LaneSenderSlot,
    bulk_slot: LaneSenderSlot,
) -> SessionEndReason
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let mut refresh_interval = refresh.as_ref().map(|refresh| {
        let mut interval = tokio::time::interval(resume_refresh_interval(refresh.window_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval
    });
    if let Some(interval) = refresh_interval.as_mut() {
        interval.tick().await;
    }
    loop {
        tokio::select! {
            biased;
            () = async {
                match refresh_interval.as_mut() {
                    Some(interval) => {
                        interval.tick().await;
                    }
                    None => std::future::pending().await,
                }
            } => {
                let Some(refresh) = refresh.as_ref() else { continue };
                let registry = refresh.session_registry.resume();
                let grant = match registry.refresh_grant(&refresh.active_session_id) {
                    Ok(grant) => grant,
                    Err(_) => {
                        if let Err(error) = registry.begin_drain(&refresh.active_session_id) {
                            tracing::error!(
                                target: SESSION,
                                ?error,
                                "resume refresh failure could not drain authority"
                            );
                        }
                        return SessionEndReason::ResumeAuthorityFailure;
                    }
                };
                if !send_successor_auth_result_or_drain(
                    &mut sink,
                    "Resume grant refreshed",
                    &grant.grant,
                    grant.window_secs,
                    false,
                    write_timeout,
                    registry,
                    &refresh.active_session_id,
                ).await {
                    return SessionEndReason::ResumeAuthorityFailure;
                }
            },
            ctrl = control.recv() => match ctrl {
                Some(WriterControl::Message(message)) => {
                    let lane = message_lane(&message);
                    if lane == LaneRole::Control {
                        if !send_ws_with_timeout(&mut sink, message, "control", write_timeout).await {
                            break;
                        }
                    } else {
                        hand_to_lane(lane, message, &input_slot, &bulk_slot);
                    }
                }
                Some(WriterControl::Barrier(complete)) => {
                    let _ = complete.send(());
                }
                None => break,
            },
        }
    }
    let _ = tokio::time::timeout(write_timeout, futures_util::SinkExt::close(&mut sink)).await;
    debug!(target: NET, "control lane writer ended");
    SessionEndReason::WriterEnded
}

fn message_lane(message: &Message) -> LaneRole {
    match message {
        Message::Text(text) => lane_for_session_text(text),
        Message::Binary(bytes) => {
            arcen_protocol::lanes::lane_for_binary_message(bytes).unwrap_or(LaneRole::Control)
        }
        Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_) => {
            LaneRole::Control
        }
    }
}

fn hand_to_lane(
    lane: LaneRole,
    message: Message,
    input_slot: &LaneSenderSlot,
    bulk_slot: &LaneSenderSlot,
) {
    let slot = match lane {
        LaneRole::InputCursor => input_slot,
        LaneRole::BulkAux => bulk_slot,
        other => {
            warn!(target: NET, lane = %other, "control writer dropped a media-lane message");
            return;
        }
    };
    let Some(record) = message_bytes(message) else {
        return;
    };
    match slot.current().map(|sender| sender.try_send(record)) {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            debug!(target: NET, lane = %lane, %error, "best-effort lane message not queued");
        }
        None => debug!(target: NET, lane = %lane, "lane not open; best-effort message dropped"),
    }
}

/// Video for every monitor (the existing fair mux) on the video lane. Waits
/// only for room in the video lane's own queue, so a stalled path pushes
/// back on the encoders through the frame queues and never on audio or
/// input. A replacement lane requests an IDR on every monitor.
async fn video_lane_writer(
    lanes: LaneConnection,
    video: VideoSource,
    write_timeout: Duration,
    write_wait_micros: Arc<AtomicU64>,
    idrs: Vec<IdrRequester>,
) -> SessionEndReason {
    let mut writer = match LocalLaneWriter::open(&lanes, LaneRole::Video).await {
        Ok(writer) => writer,
        Err(terminal) => {
            warn!(target: NET, reason = %terminal, "video lane could not be opened");
            return SessionEndReason::TransportError;
        }
    };
    info!(target: SESSION, "video runs on its own lane");
    loop {
        let Some(frame) = video.dequeue().await else {
            writer.finish();
            debug!(target: NET, "video lane writer ended");
            return SessionEndReason::WriterEnded;
        };
        let started = Instant::now();
        match tokio::time::timeout(write_timeout, writer.send(Bytes::from(frame))).await {
            Ok(Ok(LaneWriteOutcome::Reopened { request_keyframe })) => {
                info!(target: NET, "video lane reopened after a failure");
                if request_keyframe {
                    request_full_frame_idr(&idrs);
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(terminal)) => {
                warn!(target: NET, reason = %terminal, "video lane ended");
                return SessionEndReason::TransportError;
            }
            Err(_) => {
                warn!(target: NET, "video lane send timed out");
                return SessionEndReason::WriterEnded;
            }
        }
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        write_wait_micros.fetch_add(micros, Ordering::Relaxed);
    }
}

/// Audio on its own lane. Never waits: a full audio queue drops the frame
/// (the Deck's jitter buffer conceals it) and a disabled lane disables audio
/// visibly; audio is never written to any other lane. A closed audio queue
/// ends only this writer, not the attachment.
async fn audio_lane_writer(
    lanes: LaneConnection,
    audio: Arc<AudioQueue>,
    audio_control: AudioControl,
    audio_failure: mpsc::Sender<AudioStreamReason>,
) -> SessionEndReason {
    let mut writer: Option<LocalLaneWriter> = None;
    let mut dropped_full = 0_u64;
    loop {
        let Some(packet) = audio.dequeue().await else {
            if let Some(writer) = writer.as_ref() {
                writer.finish();
            }
            return std::future::pending().await;
        };
        let stream = audio_control.stream();
        if !stream.is_enabled() || stream.codec != Some(packet.codec) {
            continue;
        }
        let payload_bytes = packet.payload.len();
        let frame = media::build_audio_frame(packet.codec, &packet.payload, packet.timestamp_ms);
        if writer.is_none() {
            match LocalLaneWriter::open(&lanes, LaneRole::AudioOutput).await {
                Ok(opened) => {
                    info!(target: SESSION, "audio runs on its own lane");
                    writer = Some(opened);
                }
                Err(terminal) => return audio_lane_stopped(terminal, &audio_failure).await,
            }
        }
        let Some(lane) = writer.as_mut() else {
            continue;
        };
        match lane.try_send(Bytes::from(frame)).await {
            Ok(LaneWriteOutcome::Queued) => audio_control.record_sent(payload_bytes),
            Ok(LaneWriteOutcome::Reopened { .. }) => {
                info!(target: AUDIO, "audio lane reopened after a failure");
                audio_control.record_sent(payload_bytes);
            }
            Ok(LaneWriteOutcome::DroppedFull) => {
                dropped_full = dropped_full.saturating_add(1);
                trace!(target: AUDIO, dropped_full, "audio lane queue full; frame dropped");
            }
            Err(terminal) => return audio_lane_stopped(terminal, &audio_failure).await,
        }
    }
}

async fn audio_lane_stopped(
    terminal: LaneTerminal,
    audio_failure: &mpsc::Sender<AudioStreamReason>,
) -> SessionEndReason {
    match terminal {
        LaneTerminal::Disabled(_) => {
            warn!(
                target: AUDIO,
                "audio lane exhausted its recovery; audio disabled for this attachment"
            );
            let _ = audio_failure.send(AudioStreamReason::LaneUnavailable).await;
            std::future::pending().await
        }
        other => {
            warn!(target: AUDIO, reason = %other, "audio lane ended");
            SessionEndReason::TransportError
        }
    }
}

/// Clipboard payloads on the bulk lane the Deck opens. A clipboard queue
/// that closes ends only this writer. A message the bulk lane cannot take in
/// time abandons the rest of its transfer, visibly: the Deck gets
/// `clipboard_transfer_failed` on control and scrubs that transfer (ADR 0014),
/// instead of holding a partial one until its reassembly expires.
async fn bulk_lane_writer(
    clipboard: Arc<ClipboardWriterQueue>,
    notices: ControlSender,
    slot: LaneSenderSlot,
    write_timeout: Duration,
) -> SessionEndReason {
    loop {
        let (message, sequence) = match clipboard.pop_with_sequence().await {
            Ok(Some(next)) => next,
            Ok(None) => return std::future::pending().await,
            Err(error) => {
                warn!(target: SESSION, %error, "clipboard writer failed");
                return SessionEndReason::WriterEnded;
            }
        };
        let Some(record) = message_bytes(message) else {
            continue;
        };
        if !send_bulk_record(&slot, record, write_timeout).await {
            let reason = bulk_failure_reason(slot.current().is_some());
            clipboard.abandon(sequence);
            warn!(
                target: NET,
                event = "clipboard_transfer_abandoned",
                sequence,
                reason = ?reason,
                "clipboard transfer abandoned: bulk lane could not take it"
            );
            notify_clipboard_transfer_failed(&notices, sequence, reason);
        }
    }
}

/// A lane that is open but did not take the record in time is full; one
/// that is not open is unavailable.
const fn bulk_failure_reason(lane_open: bool) -> ClipboardTransferFailure {
    if lane_open {
        ClipboardTransferFailure::BulkLaneFull
    } else {
        ClipboardTransferFailure::BulkLaneUnavailable
    }
}

fn notify_clipboard_transfer_failed(
    notices: &ControlSender,
    sequence: u64,
    reason: ClipboardTransferFailure,
) {
    let notice = ClipboardTransferFailedMsg::new(sequence, reason);
    match serde_json::to_string(&notice) {
        Ok(text) => {
            notices.send_best_effort(Message::Text(text.into()), CLIPBOARD_TRANSFER_FAILED);
        }
        Err(error) => warn!(target: NET, %error, "clipboard_transfer_failed not serialised"),
    }
}

/// Queues one record on the bulk lane, waiting for the Deck's lane and for
/// room in its queue up to `timeout`. Returns whether it was queued.
pub(super) async fn send_bulk_record(
    slot: &LaneSenderSlot,
    record: Bytes,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            let sender = slot.wait().await;
            match sender.send(record.clone()).await {
                Ok(()) => return true,
                Err(arcen_transport::quic::LaneSendError::Closed(_)) => {
                    // A replacement lane is installed by the bulk reader.
                    tokio::task::yield_now().await;
                    if slot.current().is_some_and(|current| {
                        current.lane_generation() == sender.lane_generation()
                    }) {
                        slot.clear();
                    }
                }
                Err(arcen_transport::quic::LaneSendError::Oversize { len, max }) => {
                    warn!(target: NET, len, max, "bulk record larger than the bulk lane carries; dropped");
                    return false;
                }
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Hard USB URB frames waiting for the bulk lane.
pub(super) const USB_BULK_BACKLOG: usize = 64;

/// Hard USB frames on the bulk lane, off the control dispatcher: a full or
/// unavailable bulk lane delays only this task (and, through the bounded
/// channel, the USB helper), never control. A frame that cannot be queued
/// within `timeout` reports the bulk lane unavailable once; the dispatcher
/// then stops Hard USB and tells the Deck instead of ending the session.
pub(super) async fn usb_bulk_writer(
    mut frames: mpsc::Receiver<Vec<u8>>,
    slot: LaneSenderSlot,
    timeout: Duration,
    unavailable: mpsc::Sender<()>,
) -> SessionEndReason {
    let mut reported = false;
    while let Some(frame) = frames.recv().await {
        let record = arcen_transport::quic::session_record_bytes(&SessionRecord::Binary(frame));
        if !send_bulk_record(&slot, record, timeout).await && !reported {
            warn!(target: NET, "Hard USB URB could not be queued on the bulk lane");
            reported = true;
            let _ = unavailable.try_send(());
        }
    }
    std::future::pending().await
}

/// Releases held keys, buttons and pen state, as every input lane
/// supersession, failure and end requires.
fn release_held_input(input: &Mutex<Option<PierInput>>) {
    if let Some(controller) = lock_recover(input).as_mut() {
        if let Err(error) = controller.reset_held() {
            debug!(target: INPUT, %error, "held input release failed");
        }
    }
}

/// Where an attachment's host cursor comes from.
pub(super) enum CursorFeed {
    /// Named shapes from the X11 watcher, as `cursor_shape` JSON.
    Shapes(mpsc::Receiver<String>),
    /// A Wayland desktop's cursor (`media::wayland_cursor`): `CursorUpdate`
    /// images for a Deck that declared `cursor_image_v1`, and the named
    /// fallback (`default` or `hidden`) for one that did not.
    Images {
        facts: tokio::sync::watch::Receiver<Option<arcen_input::cursor::CursorFacts>>,
        deck_draws_images: bool,
    },
}

/// Deck input on the input/cursor lane, injected in this task only, and the
/// host cursor written back on the same lane. Topology barriers are
/// explicit in every region message (its generation), never inferred from
/// arrival order relative to the control lane.
pub(super) async fn input_lane_worker(
    lanes: LaneConnection,
    input: Arc<Mutex<Option<PierInput>>>,
    cursor_feed: Option<CursorFeed>,
    mut text_notices: Option<mpsc::Receiver<String>>,
    slot: LaneSenderSlot,
) -> SessionEndReason {
    let mut reader = PeerLaneReader::new(&lanes, LaneRole::InputCursor);
    let mut sequence = arcen_input::InputSequenceTracker::default();
    let mut pending_cursor: Option<Bytes> = None;
    let mut pending_text = TextNoticeBacklog::default();
    let mut cursor_images = arcen_input::cursor::CursorLaneOutbox::default();
    let mut fallback_sequence = 0u64;
    let (mut cursor_shapes, mut cursor_facts, deck_draws_images) = match cursor_feed {
        Some(CursorFeed::Shapes(shapes)) => (Some(shapes), None, false),
        Some(CursorFeed::Images {
            mut facts,
            deck_draws_images,
        }) => {
            // The desktop's current cursor goes out as soon as the lane opens.
            facts.mark_changed();
            (None, Some(facts), deck_draws_images)
        }
        None => (None, None, false),
    };
    let mut retry = tokio::time::interval(Duration::from_millis(20));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let reason = loop {
        tokio::select! {
            event = reader.next() => match event {
                Ok(PeerLaneEvent::Opened { lane_generation, release_held_input: release, sender, .. }) => {
                    debug!(target: INPUT, lane_generation, "input lane opened");
                    if release {
                        release_held_input(&input);
                    }
                    if let Some(sender) = sender {
                        slot.install(sender);
                    }
                    // The Deck's cache starts empty on every lane, and the
                    // current state is sent again on it.
                    cursor_images.reset_lane();
                    flush_cursor(&slot, &mut pending_cursor);
                    flush_cursor_image(&slot, &mut cursor_images);
                    pending_text.flush(&slot);
                }
                Ok(PeerLaneEvent::Lost { release_held_input: release, .. }) => {
                    // Released now, not when the Deck's replacement arrives.
                    slot.clear();
                    if release {
                        release_held_input(&input);
                    }
                }
                Ok(PeerLaneEvent::Frame(body)) => match SessionRecord::decode_body(&body) {
                    Ok(SessionRecord::Text(text)) => {
                        if lane_for_session_text(&text) != LaneRole::InputCursor {
                            warn!(target: INPUT, "non-input message on the input lane");
                            reader.fail(LaneFailure::MalformedFrame);
                            continue;
                        }
                        let mut controller = lock_recover(&input);
                        super::handle_input_json(&text, controller.as_mut(), &mut sequence);
                    }
                    Ok(SessionRecord::Ping(_) | SessionRecord::Pong(_)) => {}
                    Ok(_) => {
                        warn!(target: INPUT, "unexpected record on the input lane");
                        reader.fail(LaneFailure::MalformedFrame);
                    }
                    Err(error) => {
                        warn!(target: INPUT, %error, "malformed input lane record");
                        reader.fail(LaneFailure::MalformedFrame);
                    }
                },
                Err(terminal) => {
                    debug!(target: INPUT, reason = %terminal, "input lane ended");
                    break match terminal {
                        LaneTerminal::Finished => SessionEndReason::ClientClosed,
                        _ => SessionEndReason::TransportError,
                    };
                }
            },
            shape = async { cursor_shapes.as_mut()?.recv().await }, if cursor_shapes.is_some() => {
                match shape {
                    Some(json) => {
                        pending_cursor = message_bytes(Message::Text(json));
                        flush_cursor(&slot, &mut pending_cursor);
                    }
                    None => cursor_shapes = None,
                }
            }
            notice = async { text_notices.as_mut()?.recv().await }, if text_notices.is_some() => {
                match notice {
                    Some(json) => {
                        pending_text.push(json);
                        pending_text.flush(&slot);
                    }
                    None => text_notices = None,
                }
            }
            changed = async { cursor_facts.as_mut()?.changed().await.ok() }, if cursor_facts.is_some() => {
                let current = changed.and_then(|()| {
                    cursor_facts.as_mut().and_then(|facts| facts.borrow_and_update().clone())
                });
                match (changed, current) {
                    (None, _) => cursor_facts = None,
                    (Some(()), None) => {}
                    (Some(()), Some(facts)) if deck_draws_images => {
                        cursor_images.set(facts);
                        flush_cursor_image(&slot, &mut cursor_images);
                    }
                    (Some(()), Some(facts)) => {
                        fallback_sequence += 1;
                        let shape = arcen_protocol::messages::CursorShapeMsg {
                            shape: facts.fallback_shape(),
                            sequence: fallback_sequence,
                            ..arcen_protocol::messages::CursorShapeMsg::default()
                        };
                        if let Ok(json) = serde_json::to_string(&shape) {
                            pending_cursor = message_bytes(Message::Text(json));
                            flush_cursor(&slot, &mut pending_cursor);
                        }
                    }
                }
            }
            _ = retry.tick(), if pending_cursor.is_some() || cursor_images.is_pending() || !pending_text.is_empty() => {
                flush_cursor(&slot, &mut pending_cursor);
                flush_cursor_image(&slot, &mut cursor_images);
                pending_text.flush(&slot);
            }
        }
    };
    slot.clear();
    release_held_input(&input);
    reason
}

/// Text results and layout notices waiting for room on the input lane, in
/// order. A newer layout replaces a waiting one; results beyond the bound
/// are dropped (the Deck shows the newest refusals, not every one).
#[derive(Default)]
struct TextNoticeBacklog(VecDeque<(bool, Bytes)>);

/// Results kept waiting for the input lane.
const TEXT_NOTICE_BACKLOG: usize = 32;

impl TextNoticeBacklog {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn push(&mut self, json: String) {
        let layout = arcen_protocol::messages::msg_type(
            &serde_json::from_str::<serde_json::Value>(&json).unwrap_or_default(),
        ) == Some(arcen_protocol::messages::HOST_KEYMAP);
        let Some(record) = message_bytes(Message::Text(json)) else {
            return;
        };
        if layout {
            self.0.retain(|(waiting_layout, _)| !waiting_layout);
        }
        if self.0.len() >= TEXT_NOTICE_BACKLOG {
            if let Some(index) = self
                .0
                .iter()
                .position(|(waiting_layout, _)| !waiting_layout)
            {
                self.0.remove(index);
            }
        }
        self.0.push_back((layout, record));
    }

    fn flush(&mut self, slot: &LaneSenderSlot) {
        let Some(sender) = slot.current() else {
            return;
        };
        while let Some((layout, record)) = self.0.pop_front() {
            if let Err(error) = sender.try_send(record) {
                if let Some(record) = error.into_record() {
                    self.0.push_front((layout, record));
                }
                return;
            }
        }
    }
}

/// Writes the newest cursor shape when the input lane has room; an older
/// pending shape is replaced, never queued behind.
fn flush_cursor(slot: &LaneSenderSlot, pending: &mut Option<Bytes>) {
    // Without a lane the shape stays pending; taking it first would lose a
    // shape (such as the initial one) reported before the Deck opened it.
    let Some(sender) = slot.current() else {
        return;
    };
    let Some(record) = pending.take() else {
        return;
    };
    match sender.try_send(record) {
        Ok(()) => {}
        Err(error) => *pending = error.into_record(),
    }
}

/// Writes the newest cursor image state when the input lane has room. The
/// outbox commits (and so believes the Deck holds an inlined image) only
/// once the lane accepted the record.
fn flush_cursor_image(slot: &LaneSenderSlot, outbox: &mut arcen_input::cursor::CursorLaneOutbox) {
    let Some(sender) = slot.current() else {
        return;
    };
    let prepared = match outbox.prepare() {
        Ok(Some(prepared)) => prepared,
        Ok(None) => return,
        Err(error) => {
            warn!(target: INPUT, %error, "cursor update could not be encoded");
            return;
        }
    };
    let Some(record) = message_bytes(Message::Binary(prepared.bytes.clone())) else {
        return;
    };
    if sender.try_send(record).is_ok() {
        trace!(
            target: INPUT,
            inline_image = prepared.inline_image,
            bytes = prepared.bytes.len(),
            "cursor update sent"
        );
        outbox.commit(prepared);
    }
}

/// Microphone admission state shared by the microphone lane worker and the
/// control dispatcher (stop requests, helper events). Held only for
/// synchronous updates, never across an await.
pub(super) struct MicrophoneIngressState {
    pub(super) frames: Option<crate::microphone_input::MicrophoneFrameSender>,
    pub(super) ingress: Option<MicrophoneIngressValidator>,
    /// The negotiated stream ended; later helper events are ignored.
    pub(super) terminated: bool,
    order_warning: crate::microphone_input::RateLimitedMicrophoneWarning,
    protocol_warning: crate::microphone_input::RateLimitedMicrophoneWarning,
    codec_warning: crate::microphone_input::RateLimitedMicrophoneWarning,
    authorization_warning: crate::microphone_input::RateLimitedMicrophoneWarning,
}

impl MicrophoneIngressState {
    pub(super) fn new(
        frames: Option<crate::microphone_input::MicrophoneFrameSender>,
        ingress: Option<MicrophoneIngressValidator>,
    ) -> Self {
        Self {
            frames,
            ingress,
            terminated: false,
            order_warning: Default::default(),
            protocol_warning: Default::default(),
            codec_warning: Default::default(),
            authorization_warning: Default::default(),
        }
    }

    /// Stops the negotiated stream: no further frame reaches the helper.
    pub(super) fn stop(&mut self) {
        self.ingress = None;
        if let Some(frames) = self.frames.take() {
            frames.stop();
        }
    }
}

/// Validates one microphone frame and hands it to the helper. Returns
/// whether the Deck must be told the stream failed.
fn ingest_microphone_frame(
    state: &mut MicrophoneIngressState,
    bytes: Vec<u8>,
    stats: &Mutex<MicrophoneStatsTracker>,
    sid: &CorrelationId,
    generation: u32,
) -> bool {
    lock_recover(stats).record_received(bytes.len());
    let wire_bytes = bytes.len();
    let (Some(frames), Some(ingress)) = (state.frames.as_ref(), state.ingress.as_mut()) else {
        if let Some(suppressed) = state.authorization_warning.observe() {
            warn!(
                target: AUDIO,
                event = "mic_frame_rejected",
                sid = %sid,
                generation,
                reason = "not_negotiated",
                suppressed_since_last = suppressed,
                "unauthorized microphone frame dropped"
            );
        }
        return false;
    };
    match ingress.validate(&bytes) {
        Ok(()) => match frames.try_send(bytes) {
            Ok(()) => {
                lock_recover(stats).record_ingest(MicrophoneIngestOutcome::Accepted, wire_bytes);
                false
            }
            Err(crate::microphone_input::MicrophoneSendError::Full) => {
                lock_recover(stats).record_transport_backpressure_drop();
                false
            }
            Err(crate::microphone_input::MicrophoneSendError::Closed) => {
                lock_recover(stats).record_backend_underrun();
                state.stop();
                state.terminated = true;
                warn!(
                    target: AUDIO,
                    event = "mic_linux_helper_failure",
                    sid = %sid,
                    backend = "pulseaudio_pipe_source",
                    reason = "ipc_closed",
                    "microphone helper IPC closed"
                );
                true
            }
        },
        Err(rejection) => {
            let terminate_stream = rejection.terminates_stream();
            {
                let mut stats = lock_recover(stats);
                match rejection {
                    MicrophoneIngressRejection::Sequence(MicrophoneFrameDecision::Duplicate) => {
                        stats.record_ingest(MicrophoneIngestOutcome::DroppedDuplicate, 0);
                    }
                    MicrophoneIngressRejection::Sequence(MicrophoneFrameDecision::Late) => {
                        stats.record_ingest(MicrophoneIngestOutcome::DroppedLate, 0);
                    }
                    MicrophoneIngressRejection::Sequence(
                        MicrophoneFrameDecision::WrongGeneration,
                    ) => stats.record_ingest(MicrophoneIngestOutcome::DroppedWrongGeneration, 0),
                    MicrophoneIngressRejection::Sequence(
                        MicrophoneFrameDecision::Discontinuity,
                    ) => stats.record_ingest(MicrophoneIngestOutcome::RejectedDiscontinuity, 0),
                    MicrophoneIngressRejection::Malformed
                    | MicrophoneIngressRejection::Codec
                    | MicrophoneIngressRejection::Sequence(
                        MicrophoneFrameDecision::First
                        | MicrophoneFrameDecision::OnTime
                        | MicrophoneFrameDecision::Gap { .. },
                    ) => stats.record_decoder_error(),
                }
            }
            let warning = match rejection {
                MicrophoneIngressRejection::Malformed => &mut state.protocol_warning,
                MicrophoneIngressRejection::Codec => &mut state.codec_warning,
                MicrophoneIngressRejection::Sequence(_) => &mut state.order_warning,
            };
            if let Some(suppressed) = warning.observe() {
                warn!(
                    target: AUDIO,
                    event = "mic_frame_rejected",
                    sid = %sid,
                    generation,
                    reason = ?rejection,
                    suppressed_since_last = suppressed,
                    "Linux microphone frame rejected"
                );
            }
            if terminate_stream {
                state.stop();
                state.terminated = true;
                warn!(
                    target: AUDIO,
                    "microphone ordering discontinuity terminated the negotiated stream"
                );
            }
            terminate_stream
        }
    }
}

/// Deck microphone frames on their own lane. Video, input and control never
/// share this task; a microphone lane failure leaves the consent generation
/// untouched, and an exhausted lane stops the stream visibly.
pub(super) async fn microphone_lane_worker(
    lanes: LaneConnection,
    state: Arc<Mutex<MicrophoneIngressState>>,
    stats: Arc<Mutex<MicrophoneStatsTracker>>,
    control: ControlSender,
    generation: u32,
    sid: CorrelationId,
) -> SessionEndReason {
    let mut reader = PeerLaneReader::new(&lanes, LaneRole::Microphone);
    loop {
        match reader.next().await {
            Ok(PeerLaneEvent::Opened {
                lane_generation, ..
            }) => {
                debug!(target: AUDIO, lane_generation, "microphone lane opened");
            }
            Ok(PeerLaneEvent::Lost { .. }) => {}
            Ok(PeerLaneEvent::Frame(frame)) => {
                let report = ingest_microphone_frame(
                    &mut lock_recover(&state),
                    frame.to_vec(),
                    &stats,
                    &sid,
                    generation,
                );
                if report && !send_microphone_failure(&control, generation).await {
                    return SessionEndReason::WriterEnded;
                }
            }
            Err(LaneTerminal::Disabled(_)) => {
                warn!(
                    target: AUDIO,
                    "microphone lane exhausted its recovery; microphone stopped for this attachment"
                );
                let report = {
                    let mut state = lock_recover(&state);
                    let active = state.frames.is_some();
                    state.stop();
                    state.terminated = true;
                    active
                };
                if report {
                    let _ = send_microphone_failure(&control, generation).await;
                }
                return std::future::pending().await;
            }
            Err(terminal) => {
                debug!(target: AUDIO, reason = %terminal, "microphone lane ended");
                return std::future::pending().await;
            }
        }
    }
}

/// Clipboard and USB records from the Deck's bulk lane, forwarded to the
/// control dispatcher (which owns the clipboard reassembler and USB bridge)
/// through a bounded channel. The newest bulk sender is installed in
/// `slot` for the Pier's own bulk writes.
pub(super) async fn bulk_lane_reader(
    lanes: LaneConnection,
    slot: LaneSenderSlot,
    forward: mpsc::Sender<Message>,
    unavailable: mpsc::Sender<()>,
) -> SessionEndReason {
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
                Ok(record @ (SessionRecord::Text(_) | SessionRecord::Binary(_))) => {
                    if record.lane() != LaneRole::BulkAux {
                        warn!(target: NET, "non-bulk message on the bulk lane");
                        reader.fail(LaneFailure::MalformedFrame);
                        continue;
                    }
                    if forward.send(record_to_message(record)).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(target: NET, %error, "malformed bulk lane record");
                    reader.fail(LaneFailure::MalformedFrame);
                }
            },
            Err(LaneTerminal::Disabled(_)) => {
                warn!(
                    target: NET,
                    "bulk lane exhausted its recovery; clipboard and USB transfers disabled"
                );
                let _ = unavailable.try_send(());
                break;
            }
            Err(terminal) => {
                debug!(target: NET, reason = %terminal, "bulk lane ended");
                break;
            }
        }
    }
    slot.clear();
    std::future::pending().await
}

#[cfg(test)]
mod tests {
    //! Product-level lane isolation on a real QUIC loopback: the Linux
    //! Pier's own writers and readers against a Deck-side lane connection.

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use arcen_protocol::lanes::{LaneEndpoint, LaneRole, LaneRoleSet, SessionRecord};
    use arcen_protocol::{AudioCodec, FrameType};
    use arcen_transport::lanes::{LaneFailure, LaneState};
    use arcen_transport::quic::{
        accept_attachment, accept_session_lanes, connect_session_lanes, grant_attachment,
        lane_transport_config_arc, session_record_bytes, DirectQuicDialParams, LaneConnection,
        LaneRuntimeConfig, LaneSenderSlot, PeerLaneEvent, PeerLaneReader, SessionLane,
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::media::capenc::test_support::fake_idr;
    use crate::session::audio::EncodedAudioPacket;
    use crate::session::client::FrameQueue;

    #[tokio::test]
    async fn an_abandoned_clipboard_transfer_is_announced_on_control() {
        assert_eq!(
            bulk_failure_reason(true),
            ClipboardTransferFailure::BulkLaneFull
        );
        assert_eq!(
            bulk_failure_reason(false),
            ClipboardTransferFailure::BulkLaneUnavailable
        );
        let (notices, mut control) = ControlSender::channel(1);
        notify_clipboard_transfer_failed(&notices, 7, ClipboardTransferFailure::BulkLaneFull);
        let Some(WriterControl::Message(Message::Text(text))) = control.recv().await else {
            panic!("a control message");
        };
        let notice: ClipboardTransferFailedMsg = serde_json::from_str(&text).unwrap();
        assert_eq!(
            notice,
            ClipboardTransferFailedMsg::new(7, ClipboardTransferFailure::BulkLaneFull)
        );
    }

    #[tokio::test]
    async fn a_clipboard_transfer_the_bulk_lane_cannot_take_is_abandoned_visibly() {
        let clipboard = ClipboardWriterQueue::new();
        assert!(clipboard.enqueue(
            crate::clipboard::ClipboardItem::new(
                3,
                arcen_protocol::messages::ClipboardContentKind::TextUtf8,
                vec![b'x'; 3 * arcen_protocol::CHUNK_BYTES],
                false,
            )
            .unwrap()
        ));
        let (notices, mut control) = ControlSender::channel(4);
        // No bulk lane is ever installed: the first record times out.
        let writer = tokio::spawn(bulk_lane_writer(
            Arc::clone(&clipboard),
            notices,
            LaneSenderSlot::new(LaneRole::BulkAux),
            Duration::from_millis(20),
        ));
        let Some(WriterControl::Message(Message::Text(text))) = control.recv().await else {
            panic!("a control message");
        };
        let notice: ClipboardTransferFailedMsg = serde_json::from_str(&text).unwrap();
        assert_eq!(
            notice,
            ClipboardTransferFailedMsg::new(3, ClipboardTransferFailure::BulkLaneUnavailable)
        );
        // Only the offer was attempted: the rest of the transfer was dropped,
        // so no second notice follows for it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(control.try_recv().is_err());
        writer.abort();
    }

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
        let certified =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
        let cert = rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        );
        let policy = arcen_transport::BoundedTransportPolicy::default();
        let mut server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("server tls");
        server_tls.alpn_protocols = vec![b"arcen-lanes-test".to_vec()];
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).expect("quic server"),
        ));
        server_config.transport_config(lane_transport_config_arc(&policy, LaneEndpoint::Pier));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("root");
        let mut client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"arcen-lanes-test".to_vec()];
        let mut client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).expect("quic client"),
        ));
        client_config.transport_config(lane_transport_config_arc(&policy, LaneEndpoint::Deck));
        let server = quinn::Endpoint::server(
            server_config,
            "127.0.0.1:0".parse::<SocketAddr>().expect("addr"),
        )
        .expect("server");
        let address = server.local_addr().expect("local addr");
        let client = quinn::Endpoint::client("127.0.0.1:0".parse().expect("addr")).expect("client");
        let accepting = server.clone();
        let accept = tokio::spawn(async move {
            let connection = accepting
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("connection");
            accept_session_lanes(connection, LaneRuntimeConfig::new(LaneEndpoint::Pier), STEP)
                .await
                .expect("control lane")
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
        .expect("deck lanes");
        let (pier, pier_control) = accept.await.expect("accept task");
        let grant = grant_attachment(
            &pier,
            arcen_transport::quic::next_attachment_generation(),
            LaneRoleSet::all(),
        )
        .expect("grant");
        accept_attachment(&deck, &grant).expect("accept grant");
        Pair {
            _server: server,
            _client: client,
            pier,
            deck,
            _pier_control: pier_control,
            _deck_control: deck_control,
        }
    }

    fn audio_control() -> AudioControl {
        let policy = arcen_media::audio::AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let stream = policy.resolve(None, true, 128);
        AudioControl::new(true, false, true, None, stream)
    }

    struct Running {
        writers: JoinHandle<SessionEndReason>,
        video: Arc<FrameQueue>,
        audio: Arc<AudioQueue>,
        idr_rx: mpsc::UnboundedReceiver<()>,
        _control_tx: ControlSender,
    }

    fn start_writers(pair: &Pair) -> Running {
        let (idr, idr_rx) = fake_idr();
        let video = Arc::new(FrameQueue::new(idr.clone()));
        let audio = Arc::new(AudioQueue::new());
        let clipboard = ClipboardWriterQueue::new();
        clipboard.close();
        let (control_tx, control_rx) = ControlSender::channel(4);
        let (audio_failure, _audio_failure_rx) = mpsc::channel(1);
        let writers = spawn_lane_writers(LaneWriters {
            lanes: pair.pier.clone(),
            control_sink: futures_util::sink::drain(),
            control: control_rx,
            refresh: None,
            write_timeout: Duration::from_secs(30),
            video: VideoSource::Single(Arc::clone(&video)),
            video_write_wait_micros: Arc::new(AtomicU64::new(0)),
            idrs: vec![idr],
            audio: Arc::clone(&audio),
            audio_control: audio_control(),
            audio_failure,
            clipboard,
            clipboard_notices: control_tx.clone(),
            input_slot: LaneSenderSlot::new(LaneRole::InputCursor),
            bulk_slot: LaneSenderSlot::new(LaneRole::BulkAux),
        });
        Running {
            writers,
            video,
            audio,
            idr_rx,
            _control_tx: control_tx,
        }
    }

    fn audio_packet(sequence: u32) -> EncodedAudioPacket {
        EncodedAudioPacket {
            codec: AudioCodec::Pcm,
            payload: vec![u8::try_from(sequence % 251).unwrap_or(0); 640],
            timestamp_ms: sequence.saturating_mul(20),
        }
    }

    async fn next_frame(reader: &mut PeerLaneReader) -> Bytes {
        loop {
            match tokio::time::timeout(STEP, reader.next())
                .await
                .expect("lane event in time")
                .expect("lane open")
            {
                PeerLaneEvent::Frame(frame) => return frame,
                PeerLaneEvent::Opened { .. } | PeerLaneEvent::Lost { .. } => {}
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn audio_and_input_flow_while_the_deck_stops_reading_video() {
        let pair = pair().await;
        let running = start_writers(&pair);
        // A deep video backlog the Deck never reads: the video lane runs out
        // of stream credit and its writer blocks.
        for index in 0..64_u8 {
            running.video.enqueue(
                vec![FrameType::VideoH264 as u8, index].repeat(256 * 1024),
                index == 0,
            );
        }
        let mut deck_audio = PeerLaneReader::new(&pair.deck, LaneRole::AudioOutput);

        // Input/cursor: the Deck opens its lane; the Pier's input worker
        // follows it and writes cursor shapes back.
        let (cursor_tx, cursor_rx) = mpsc::channel(4);
        let input_slot = LaneSenderSlot::new(LaneRole::InputCursor);
        let input_worker = tokio::spawn(input_lane_worker(
            pair.pier.clone(),
            Arc::new(Mutex::new(None)),
            Some(CursorFeed::Shapes(cursor_rx)),
            None,
            input_slot,
        ));
        let mut deck_input = SessionLane::split(
            tokio::time::timeout(STEP, pair.deck.open_lane(LaneRole::InputCursor))
                .await
                .expect("open in time")
                .expect("input lane"),
        )
        .expect("session lane");

        tokio::time::sleep(Duration::from_millis(200)).await;
        let video_status = pair.pier.status();
        assert!(
            video_status.role(LaneRole::Video).queued_records > 0,
            "the video lane must be backed up for this test to mean anything"
        );

        let mut worst_audio = Duration::ZERO;
        let mut worst_cursor = Duration::ZERO;
        for sequence in 0..25_u32 {
            let sent = Instant::now();
            running.audio.enqueue(audio_packet(sequence));
            let frame = next_frame(&mut deck_audio).await;
            worst_audio = worst_audio.max(sent.elapsed());
            assert_eq!(frame[0], FrameType::Audio as u8);

            let key = SessionRecord::Text(format!(
                "{{\"type\":\"key_event\",\"scan_code\":4,\"pressed\":{},\"sequence\":{}}}",
                sequence % 2 == 0,
                sequence + 1
            ));
            deck_input
                .sender
                .try_send(session_record_bytes(&key))
                .expect("input lane has room for every edge");
            let sent = Instant::now();
            cursor_tx
                .send(format!(
                    "{{\"type\":\"cursor_shape\",\"shape\":\"arrow\",\"n\":{sequence}}}"
                ))
                .await
                .expect("cursor channel");
            let record = tokio::time::timeout(STEP, deck_input.reader.next_record())
                .await
                .expect("cursor in time")
                .expect("cursor record");
            worst_cursor = worst_cursor.max(sent.elapsed());
            assert!(matches!(record, SessionRecord::Text(text) if text.contains("cursor_shape")));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        println!(
            "video blocked: worst audio {:.1} ms, worst cursor {:.1} ms",
            worst_audio.as_secs_f64() * 1e3,
            worst_cursor.as_secs_f64() * 1e3
        );
        assert!(worst_audio < Duration::from_millis(250), "{worst_audio:?}");
        assert!(
            worst_cursor < Duration::from_millis(250),
            "{worst_cursor:?}"
        );
        // Video is still backed up, and nothing ended.
        assert!(pair.pier.status().role(LaneRole::Video).queued_records > 0);
        assert!(!running.writers.is_finished());
        assert!(!input_worker.is_finished());
        running.writers.abort();
        input_worker.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_video_lane_failure_requests_an_idr_and_leaves_audio_alone() {
        let pair = pair().await;
        let mut running = start_writers(&pair);
        let mut deck_video = PeerLaneReader::new(&pair.deck, LaneRole::Video);
        let mut deck_audio = PeerLaneReader::new(&pair.deck, LaneRole::AudioOutput);
        running
            .video
            .enqueue(vec![FrameType::VideoH264 as u8, 1], true);
        running.audio.enqueue(audio_packet(0));
        assert_eq!(
            next_frame(&mut deck_video).await.as_ref(),
            &[FrameType::VideoH264 as u8, 1]
        );
        next_frame(&mut deck_audio).await;
        let audio_generation = deck_audio.lane_generation();
        let LaneState::Open(video_generation) = pair.pier.lane_state(LaneRole::Video) else {
            panic!("video lane open");
        };

        let _ =
            pair.pier
                .report_failure(LaneRole::Video, video_generation, LaneFailure::WriteFailed);
        running
            .video
            .enqueue(vec![FrameType::VideoH264 as u8, 2], false);
        tokio::time::timeout(STEP, running.idr_rx.recv())
            .await
            .expect("IDR requested in time")
            .expect("IDR request");
        // The keyframe step comes with the loss or with the replacement.
        let mut replacement = false;
        loop {
            match tokio::time::timeout(STEP, deck_video.next())
                .await
                .expect("video event")
                .expect("video lane")
            {
                PeerLaneEvent::Lost {
                    request_keyframe, ..
                } => replacement |= request_keyframe,
                PeerLaneEvent::Opened {
                    request_keyframe, ..
                } => {
                    replacement |= request_keyframe;
                    break;
                }
                PeerLaneEvent::Frame(_) => {}
            }
        }
        assert!(
            replacement,
            "the Deck must wait for a keyframe on the new lane"
        );

        // Audio keeps its lane generation and keeps flowing.
        running.audio.enqueue(audio_packet(1));
        next_frame(&mut deck_audio).await;
        assert_eq!(deck_audio.lane_generation(), audio_generation);
        running.writers.abort();
    }

    #[tokio::test]
    async fn an_unavailable_bulk_lane_stops_usb_without_ending_anything() {
        // No Deck bulk lane: the writer waits only for its own lane, reports
        // it unavailable once, and keeps accepting (and dropping) frames.
        let slot = LaneSenderSlot::new(LaneRole::BulkAux);
        let (frames_tx, frames_rx) = mpsc::channel(USB_BULK_BACKLOG);
        let (unavailable_tx, mut unavailable_rx) = mpsc::channel(4);
        let writer = tokio::spawn(usb_bulk_writer(
            frames_rx,
            slot,
            Duration::from_millis(20),
            unavailable_tx,
        ));
        for _ in 0..3 {
            frames_tx
                .send(vec![FrameType::UsbBridgeUrbComplete as u8, 0])
                .await
                .unwrap();
        }
        tokio::time::timeout(STEP, unavailable_rx.recv())
            .await
            .expect("reported in time")
            .expect("reported");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(unavailable_rx.try_recv().is_err(), "reported once");
        assert!(
            !writer.is_finished(),
            "the writer never ends the attachment"
        );
        writer.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_closed_audio_queue_ends_neither_video_nor_the_attachment() {
        let pair = pair().await;
        let running = start_writers(&pair);
        let mut deck_video = PeerLaneReader::new(&pair.deck, LaneRole::Video);
        running.audio.close();
        tokio::time::sleep(Duration::from_millis(50)).await;
        running
            .video
            .enqueue(vec![FrameType::VideoH264 as u8, 9], true);
        assert_eq!(
            next_frame(&mut deck_video).await.as_ref(),
            &[FrameType::VideoH264 as u8, 9]
        );
        assert!(!running.writers.is_finished());
        running.video.close();
        let reason = tokio::time::timeout(STEP, running.writers)
            .await
            .expect("writers end once video ends")
            .expect("supervisor");
        assert_eq!(reason, SessionEndReason::WriterEnded);
    }

    /// The session agent's side of the input socket, with a compositor that
    /// records what it was asked to inject.
    struct RecordingCompositor(Mutex<Vec<crate::session::wayland::InputCommand>>);

    impl crate::session::wayland::input::CompositorInput for RecordingCompositor {
        type Error = std::convert::Infallible;

        fn notify(
            &self,
            command: &crate::session::wayland::InputCommand,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
            lock_recover(&self.0).push(command.clone());
            std::future::ready(Ok(()))
        }
    }

    impl RecordingCompositor {
        async fn wait_for(&self, wanted: &crate::session::wayland::InputCommand) {
            tokio::time::timeout(STEP, async {
                while !lock_recover(&self.0).contains(wanted) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{wanted:?} never reached the compositor: {:?}",
                    lock_recover(&self.0)
                )
            });
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wayland_input_reaches_the_session_and_lane_loss_releases_it() {
        use std::os::unix::fs::PermissionsExt;

        use crate::input::wayland::{WaylandInjector, WaylandInputTarget};
        use crate::session::wayland::input::InputService;
        use crate::session::wayland::{
            InputBackendKind, InputCommand, InputSessionFacts, ReadyStream, INPUT_SOCKET_NAME,
        };

        let dir = std::env::current_exe()
            .expect("test binary")
            .with_file_name(format!("arcen-wayland-input-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("runtime dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
        let socket = dir.join(INPUT_SOCKET_NAME);
        let uid = nix::unistd::Uid::effective().as_raw();
        let stream = ReadyStream {
            connector: "Meta-0".into(),
            node_id: 44,
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
        };
        let facts = InputSessionFacts {
            generation: 1,
            streams: vec![stream.clone()],
            device_types: 7,
            remote_desktop_started: true,
            backend: InputBackendKind::RemoteDesktopDbus,
            eis_offered: Some(false),
            stylus: None,
            connection: 0,
            text: None,
        };
        let service = Arc::new(InputService::bind(&socket, uid).expect("bind"));
        let compositor = Arc::new(RecordingCompositor(Mutex::new(Vec::new())));
        let agent = tokio::spawn({
            let service = Arc::clone(&service);
            let compositor = Arc::clone(&compositor);
            async move { service.serve(&*compositor, &facts.into()).await }
        });

        let target =
            WaylandInputTarget::for_tests(socket, uid, Some(std::process::id()), 1, vec![stream]);
        let (injector, _stats) = WaylandInjector::connect(target).await.expect("injector");
        let input = Arc::new(Mutex::new(Some(PierInput::Wayland(injector))));

        let pair = pair().await;
        let input_worker = tokio::spawn(input_lane_worker(
            pair.pier.clone(),
            Arc::clone(&input),
            None,
            None,
            LaneSenderSlot::new(LaneRole::InputCursor),
        ));
        let deck_input = SessionLane::split(
            tokio::time::timeout(STEP, pair.deck.open_lane(LaneRole::InputCursor))
                .await
                .expect("open in time")
                .expect("input lane"),
        )
        .expect("session lane");
        for text in [
            "{\"type\":\"key_event\",\"scan_code\":65,\"pressed\":true,\"sequence\":1}",
            "{\"type\":\"mouse_move\",\"x\":0.5,\"y\":1.0,\"sequence\":2}",
            "{\"type\":\"mouse_button\",\"x\":0.5,\"y\":1.0,\"button\":1,\"pressed\":true,\"sequence\":3}",
        ] {
            deck_input
                .sender
                .try_send(session_record_bytes(&SessionRecord::Text(text.to_owned())))
                .expect("queued");
        }
        compositor
            .wait_for(&InputCommand::Key {
                keycode: 30,
                pressed: true,
            })
            .await;
        compositor
            .wait_for(&InputCommand::PointerAbsolute {
                output: "Meta-0".into(),
                generation: 1,
                x: 959.5,
                y: 1079.0,
            })
            .await;
        compositor
            .wait_for(&InputCommand::PointerButton {
                button: 0x110,
                pressed: true,
            })
            .await;

        // A malformed record fails the input lane: the Pier releases what
        // the Deck held at once, not when the Deck reopens the lane.
        deck_input
            .sender
            .try_send(session_record_bytes(&SessionRecord::Text(
                "{\"type\":\"quality_settings\"}".to_owned(),
            )))
            .expect("queued");
        compositor
            .wait_for(&InputCommand::Key {
                keycode: 30,
                pressed: false,
            })
            .await;
        compositor
            .wait_for(&InputCommand::PointerButton {
                button: 0x110,
                pressed: false,
            })
            .await;
        assert!(pair.pier.ended().is_none());
        input_worker.abort();
        agent.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn text_notices_reach_the_deck_on_its_input_lane_in_order() {
        let pair = pair().await;
        let (notices_tx, notices_rx) = mpsc::channel(8);
        // Waiting before the Deck opens its lane: the older layout is
        // replaced by the newer one, the result keeps its place.
        notices_tx
            .send("{\"type\":\"host_keymap\",\"generation\":1}".to_owned())
            .await
            .unwrap();
        notices_tx
            .send("{\"type\":\"text_commit_result\",\"sequence\":4}".to_owned())
            .await
            .unwrap();
        notices_tx
            .send("{\"type\":\"host_keymap\",\"generation\":2}".to_owned())
            .await
            .unwrap();
        let input_worker = tokio::spawn(input_lane_worker(
            pair.pier.clone(),
            Arc::new(Mutex::new(None)),
            None,
            Some(notices_rx),
            LaneSenderSlot::new(LaneRole::InputCursor),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut deck_input = SessionLane::split(
            tokio::time::timeout(STEP, pair.deck.open_lane(LaneRole::InputCursor))
                .await
                .expect("open in time")
                .expect("input lane"),
        )
        .expect("session lane");
        let mut received = Vec::new();
        for _ in 0..2 {
            let record = tokio::time::timeout(STEP, deck_input.reader.next_record())
                .await
                .expect("notice in time")
                .expect("notice record");
            let SessionRecord::Text(text) = record else {
                panic!("text notice expected");
            };
            received.push(text);
        }
        assert!(received[0].contains("text_commit_result"), "{received:?}");
        assert!(received[1].contains("\"generation\":2"), "{received:?}");
        input_worker.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_non_input_message_on_the_input_lane_fails_only_that_lane() {
        let pair = pair().await;
        let input_worker = tokio::spawn(input_lane_worker(
            pair.pier.clone(),
            Arc::new(Mutex::new(None)),
            None,
            None,
            LaneSenderSlot::new(LaneRole::InputCursor),
        ));
        let deck_input = SessionLane::split(
            tokio::time::timeout(STEP, pair.deck.open_lane(LaneRole::InputCursor))
                .await
                .expect("open in time")
                .expect("input lane"),
        )
        .expect("session lane");
        let generation = deck_input.lane_generation;
        deck_input
            .sender
            .try_send(session_record_bytes(&SessionRecord::Text(
                "{\"type\":\"quality_settings\"}".to_owned(),
            )))
            .expect("queued");
        tokio::time::timeout(STEP, async {
            while matches!(
                pair.pier.lane_state(LaneRole::InputCursor),
                LaneState::Absent
            ) || pair.pier.lane_state(LaneRole::InputCursor) == LaneState::Open(generation)
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("lane failure in time");
        let state = pair.pier.lane_state(LaneRole::InputCursor);
        assert!(
            matches!(
                state,
                LaneState::Recovering {
                    failure: LaneFailure::MalformedFrame,
                    ..
                }
            ),
            "{state:?}"
        );
        assert!(pair.pier.ended().is_none());
        assert!(!input_worker.is_finished());
        input_worker.abort();
    }

    /// Cursors from one desktop (one registry, so distinct ids).
    fn desktop_cursors(seeds: &[u8]) -> Vec<arcen_input::cursor::CursorFacts> {
        use arcen_input::cursor::CursorAggregate;
        use arcen_media::capenc_stream::{CapencCursorEvent, CapencCursorSprite};
        let mut aggregate = CursorAggregate::default();
        seeds
            .iter()
            .map(|seed| {
                let image = arcen_media::cursor::CursorImage::from_straight_rgba(
                    4,
                    4,
                    (1, 1),
                    [*seed, 9, 9, 255].repeat(16),
                )
                .expect("image");
                aggregate.apply(
                    0,
                    1,
                    &CapencCursorEvent {
                        on_output: true,
                        sprite: CapencCursorSprite::Image(image),
                    },
                );
                aggregate.current().cloned().expect("cursor")
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wayland_cursor_images_reach_a_capable_deck_and_names_reach_an_old_one() {
        use arcen_input::cursor::DeckCursorState;
        for deck_draws_images in [true, false] {
            let cursors = desktop_cursors(&[1, 2, 1]);
            let (facts_tx, facts_rx) = tokio::sync::watch::channel(Some(cursors[0].clone()));
            let pair = pair().await;
            let input_slot = LaneSenderSlot::new(LaneRole::InputCursor);
            let input_worker = tokio::spawn(input_lane_worker(
                pair.pier.clone(),
                Arc::new(Mutex::new(None)),
                Some(CursorFeed::Images {
                    facts: facts_rx,
                    deck_draws_images,
                }),
                None,
                input_slot,
            ));
            let mut deck_input = SessionLane::split(
                tokio::time::timeout(STEP, pair.deck.open_lane(LaneRole::InputCursor))
                    .await
                    .expect("open in time")
                    .expect("input lane"),
            )
            .expect("session lane");
            // QUIC makes the Deck's lane visible to the Pier with its first
            // bytes; a Deck sends input (here a ping) once its lane is open.
            deck_input
                .sender
                .try_send(session_record_bytes(&SessionRecord::Ping(vec![1])))
                .expect("queued");
            let mut deck = DeckCursorState::default();
            async fn next(lane: &mut SessionLane) -> SessionRecord {
                tokio::time::timeout(STEP, lane.reader.next_record())
                    .await
                    .expect("cursor in time")
                    .expect("cursor record")
            }
            // The desktop's current cursor arrives as soon as the lane opens.
            match next(&mut deck_input).await {
                SessionRecord::Binary(bytes) if deck_draws_images => {
                    assert_eq!(bytes[0], FrameType::CursorUpdate as u8);
                    assert!(deck.apply(&bytes).expect("valid record"));
                    assert!(matches!(
                        deck.current(),
                        arcen_input::cursor::DeckCursor::Image { .. }
                    ));
                }
                SessionRecord::Text(text) if !deck_draws_images => {
                    assert!(text.contains("\"cursor_shape\""), "{text}");
                    assert!(text.contains("\"default\""), "{text}");
                }
                other => panic!("unexpected first cursor record {other:?}"),
            }
            facts_tx.send_replace(Some(cursors[1].clone()));
            let second = next(&mut deck_input).await;
            facts_tx.send_replace(Some(cursors[2].clone()));
            let third = next(&mut deck_input).await;
            if deck_draws_images {
                let (SessionRecord::Binary(second), SessionRecord::Binary(third)) = (second, third)
                else {
                    panic!("binary cursor records expected");
                };
                assert!(second.len() > arcen_protocol::cursor::CURSOR_UPDATE_HEADER_SIZE);
                assert_eq!(
                    third.len(),
                    arcen_protocol::cursor::CURSOR_UPDATE_HEADER_SIZE,
                    "an image the Deck holds travels by id only"
                );
                deck.apply(&second).expect("valid");
                deck.apply(&third).expect("valid");
                assert_eq!(deck.unknown_images(), 0);
            } else {
                assert!(matches!(second, SessionRecord::Text(_)));
                assert!(matches!(third, SessionRecord::Text(_)));
            }
            input_worker.abort();
        }
    }
}
