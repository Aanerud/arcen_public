//! Streaming encoded frames to a connected Deck.
//!
//! This is the piece that joins capture and encode to the wire. Everything
//! before it produced frames that went nowhere; a Deck could connect and
//! handshake, but its window stayed black.
//!
//! Each access unit is sent as one binary message: the shared video header
//! followed by Annex B bytes. The header is built from the resolved plan
//! rather than from what the encoder happens to have produced, so codec,
//! chroma and bit depth on the wire always describe what a decoder is about to
//! be handed.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use serde::Serialize;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::capture::{
    CaptureConfig, CaptureError, CapturePixelFormat, CaptureSession, CapturedFrame,
};
use crate::encode::{EncodedAccessUnit, Encoder, EncoderCodec, EncoderConfig};
use crate::net::PierSocket;

/// Authoritative input-mode result messages sent once, before media/input flow starts.
#[derive(Debug, Clone, Default)]
pub struct InputModeResults {
    pub cursor: arcen_protocol::messages::CursorModeResultMsg,
    pub tablet: arcen_protocol::messages::TabletModeResultMsg,
}

/// How long to wait for a frame before deciding the display is idle.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
const FRAME_TIMEOUT_ENV: &str = "ARCEN_FRAME_TIMEOUT_MS";
/// How long a new capture may go without proving it can produce an image.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_FRAME_TIMEOUT_ENV: &str = "ARCEN_FIRST_FRAME_TIMEOUT_MS";
/// How long any post-handshake write may block session progress.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT_ENV: &str = "ARCEN_WRITE_TIMEOUT_MS";
/// How often client recovery requests may force a fresh keyframe.
const FULL_FRAME_KEYFRAME_GUARD: Duration = Duration::from_millis(500);
/// How often the local pasteboard is examined.
///
/// The pasteboard offers no change notification, so it has to be asked. Twice
/// a second is fast enough that a copy feels immediate and slow enough that an
/// idle session costs nothing.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);
/// How often the session picks up a cursor shape change to forward.
const CURSOR_POLL: Duration = Duration::from_millis(60);
const CLIPBOARD_SEND_TICK: Duration = Duration::from_millis(1);

/// How often captured audio is drained and sent.
///
/// Short enough that a listener does not hear the cadence as chopping, and
/// long enough not to wake the loop for nothing on a silent desktop.
const AUDIO_POLL: Duration = Duration::from_millis(20);
const PRODUCER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// What the stream measured while it ran.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StreamStats {
    /// Frames delivered by capture.
    pub frames_captured: u64,
    /// Frames `ScreenCaptureKit` produced that this host could not keep up
    /// with, and shed.
    ///
    /// Shedding is the correct back-pressure — blocking the callback stalls
    /// the compositor for every application on the machine — but it was silent,
    /// and a host dropping half its frames reads on the wire exactly like a
    /// still desktop. Both give a low delivered rate; only this tells them
    /// apart.
    pub frames_dropped: u64,
    /// Audio packets sent to the client.
    pub audio_packets_sent: u64,
    /// Frames not encoded because the compositor reported no change.
    ///
    /// A still desktop should suppress nearly everything: that is damage
    /// tracking working, not a fault. A session that suppresses nothing while
    /// idle is one that is spending its bandwidth re-sending a still picture.
    pub frames_suppressed: u64,
    /// Audio packets dropped because they were too old to play in time.
    ///
    /// Reported rather than silent: a non-zero count says the host could not
    /// keep up with its own capture, which is a different fault from audio
    /// that never arrived.
    pub audio_packets_dropped: u64,
    /// Keyframes the client asked for.
    ///
    /// A Deck asking repeatedly is a Deck that cannot decode what it is being
    /// sent, so the request is counted as well as forwarded to the encoder.
    pub full_frame_requests: u64,
    /// Frames the encoder produced bytes for.
    pub frames_encoded: u64,
    /// Frames written to the socket.
    pub frames_sent: u64,
    /// Access units a decoder could start from.
    pub keyframes: u64,
    /// Total payload bytes, headers included.
    pub bytes_sent: u64,
    /// How long the stream ran.
    pub elapsed_ms: u128,
    /// Frames per second actually delivered to the client.
    pub sent_fps: f64,
    /// Mean time from capture to the frame being handed to the socket.
    pub mean_frame_ms: f64,
    /// Worst capture-to-socket time. This is the stutter a viewer notices.
    pub max_frame_ms: f64,
    /// Mean time the producer waited for capture to hand over a frame.
    pub mean_capture_wait_ms: f64,
    /// Mean time spent encoding.
    pub mean_encode_ms: f64,
    /// Mean time spent writing to the socket.
    ///
    /// Measured separately so the limiting stage is a fact rather than a
    /// guess: optimising the wrong one buys nothing.
    pub mean_send_ms: f64,
    /// Mean time a finished frame waited for the writer.
    ///
    /// The Deck reports how old a frame was when it presented it. This is the
    /// part of that age this host is responsible for, so the two can be
    /// subtracted rather than argued about.
    pub mean_queue_ms: f64,
    /// What the client's input did while the stream ran.
    pub input: crate::input_session::InputStats,
    /// What the clipboard did while the stream ran.
    pub clipboard: crate::clipboard_session::ClipboardStats,
    /// Clipboard chunks the host refused or could not reassemble.
    pub clipboard_rejected: u64,
    /// Binary frames naming a capability this host has not built.
    pub unsupported_binary: u64,
    /// The most recent such capability, so the gap has a name.
    pub last_unsupported_binary: Option<UnsupportedBinary>,
}

/// Why a stream stopped.
#[derive(Debug)]
pub enum StreamError {
    /// Capture could not start or failed mid-stream.
    Capture(CaptureError),
    /// The encoder refused the stream.
    Encode(String),
    /// The client went away.
    PeerGone(String),
    /// A post-handshake write made no progress.
    WriteTimeout,
    /// Input could not be injected.
    Input(String),
    /// The clipboard service could not be established or framed.
    Clipboard(String),
    /// A control message could not be encoded or decoded.
    Protocol(String),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capture(error) => write!(formatter, "capture: {error}"),
            Self::Encode(detail) => write!(formatter, "encode: {detail}"),
            Self::PeerGone(detail) => write!(formatter, "client gone: {detail}"),
            Self::WriteTimeout => formatter.write_str("client write timed out"),
            Self::Input(detail) => write!(formatter, "input: {detail}"),
            Self::Clipboard(detail) => write!(formatter, "clipboard: {detail}"),
            Self::Protocol(detail) => write!(formatter, "protocol: {detail}"),
        }
    }
}

impl std::error::Error for StreamError {}

/// Describes the stream this encoder is producing, in the shared vocabulary.
///
/// The translation from macOS capture formats to the wire's colour description
/// is the only part of framing that is this host's business. Frame type
/// selection, flag packing and header layout live in
/// [`arcen_media::video::video_header`], beside the same decisions every other
/// Pier makes — they were written out three times, and a Deck refuses a frame
/// whose header disagrees with the plan it was promised.
/// The capture facts a frame header states: its layout and its dynamic range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireShape {
    /// The captured layout.
    pub format: CapturePixelFormat,
    /// SDR, or which HDR the capture was asked for.
    pub dynamic_range: crate::capture::CaptureDynamicRange,
}

impl WireShape {
    /// The shape of what `capture` delivers.
    #[must_use]
    pub const fn of(capture: &CaptureConfig) -> Self {
        Self {
            format: capture.pixel_format,
            dynamic_range: capture.dynamic_range,
        }
    }
}

impl From<CapturePixelFormat> for WireShape {
    /// An SDR capture of `format`.
    fn from(format: CapturePixelFormat) -> Self {
        Self {
            format,
            dynamic_range: crate::capture::CaptureDynamicRange::Sdr,
        }
    }
}

#[must_use]
const fn wire_profile(
    codec: EncoderCodec,
    shape: WireShape,
) -> arcen_media::video::VideoWireProfile {
    let format = shape.format;
    let (chroma, bit_depth) = match format {
        // BGRA is captured, never coded; the encoder converts to 4:2:0.
        CapturePixelFormat::Bgra8 | CapturePixelFormat::Nv12VideoRange => (
            arcen_media::ChromaSubsampling::Yuv420,
            arcen_media::BitDepth::Eight,
        ),
        CapturePixelFormat::Nv12TenBitVideoRange => (
            arcen_media::ChromaSubsampling::Yuv420,
            arcen_media::BitDepth::Ten,
        ),
        CapturePixelFormat::FourFourFourTenBit
        | CapturePixelFormat::FourFourFourTenBitFullRange => (
            arcen_media::ChromaSubsampling::Yuv444,
            arcen_media::BitDepth::Ten,
        ),
    };
    // The range is the capture's: VideoToolbox carries the surface's range
    // into the stream, measured by reading the SPS back.
    let range = if matches!(format, CapturePixelFormat::FourFourFourTenBitFullRange) {
        arcen_media::ColorRange::Full
    } else {
        arcen_media::ColorRange::Limited
    };
    arcen_media::video::VideoWireProfile {
        codec: match codec {
            EncoderCodec::H264 => arcen_media::video::FramedVideoCodec::H264,
            EncoderCodec::Hevc => arcen_media::video::FramedVideoCodec::H265,
        },
        chroma,
        bit_depth,
        range,
        // HDR is captured and encoded with the BT.2020 matrix; SDR, of any
        // depth, with BT.709.
        matrix: match shape.dynamic_range {
            crate::capture::CaptureDynamicRange::Sdr => arcen_media::ColorMatrix::Bt709,
            crate::capture::CaptureDynamicRange::HdrLocalDisplay
            | crate::capture::CaptureDynamicRange::HdrCanonicalDisplay => {
                arcen_media::ColorMatrix::Bt2020Ncl
            }
        },
    }
}

/// Builds the header for one access unit on the legacy single-monitor path.
#[must_use]
pub fn header_for(
    unit: &EncodedAccessUnit,
    codec: EncoderCodec,
    shape: impl Into<WireShape>,
    timestamp_ms: u32,
) -> Vec<u8> {
    let header = arcen_media::video::video_header(
        wire_profile(codec, shape.into()),
        arcen_media::video::VideoWireRoute::default(),
        unit.keyframe,
        timestamp_ms,
    );
    arcen_protocol::encode_video_header(header)
}

/// Builds the region-video header for one monitor's access unit.
#[must_use]
pub fn region_header_for(
    unit: &EncodedAccessUnit,
    codec: EncoderCodec,
    shape: impl Into<WireShape>,
    timestamp_ms: u32,
    monitor_id: arcen_media::SessionMonitorId,
    topology_generation: arcen_media::TopologyGeneration,
    stream_epoch: arcen_media::MediaStreamEpoch,
) -> Vec<u8> {
    let header = arcen_media::video::video_header(
        wire_profile(codec, shape.into()),
        arcen_media::video::VideoWireRoute {
            monitor_id: monitor_id.get(),
            topology_generation: topology_generation.get(),
            stream_epoch: stream_epoch.get(),
        },
        unit.keyframe,
        timestamp_ms,
    );
    arcen_protocol::encode_video_header(header)
}

fn clipboard_wire_message(
    message: arcen_protocol::clipboard::ClipboardWireMessage,
) -> Result<Message, StreamError> {
    match message {
        arcen_protocol::clipboard::ClipboardWireMessage::Offer(offer) => {
            let offer = serde_json::to_string(&offer).map_err(|error| {
                StreamError::Clipboard(format!("serialize clipboard offer: {error}"))
            })?;
            Ok(Message::Text(offer))
        }
        arcen_protocol::clipboard::ClipboardWireMessage::Chunk(frame) => Ok(Message::Binary(frame)),
    }
}

/// One host-to-client clipboard transfer, yielded one wire message per turn.
struct ClipboardSender {
    active: Option<arcen_protocol::clipboard::ClipboardCursor>,
}

impl ClipboardSender {
    const fn new() -> Self {
        Self { active: None }
    }

    fn poll_latest(&mut self, clipboard: Option<&crate::clipboard_session::ClipboardWorker>) {
        let Some(clipboard) = clipboard else {
            return;
        };
        let Ok(mut slot) = clipboard.outgoing.lock() else {
            return;
        };
        let Some(item) = slot.take() else {
            return;
        };
        if self
            .active
            .as_ref()
            .is_none_or(|active| item.sequence > active.transfer().sequence())
        {
            self.active = Some(item.payload.into_cursor());
        }
    }

    const fn has_work(&self) -> bool {
        self.active.is_some()
    }

    async fn send_one(
        &mut self,
        writer: &tokio::sync::mpsc::Sender<Message>,
    ) -> Result<(), StreamError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let Some(message) = active
            .next_message()
            .transpose()
            .map_err(|error| StreamError::Clipboard(error.to_string()))?
        else {
            self.active = None;
            return Ok(());
        };
        let finished = active.finished();
        let message = clipboard_wire_message(message)?;
        enqueue(writer, message).await?;
        if finished {
            self.active = None;
        }
        Ok(())
    }
}

/// Drains captured audio and sends it, returning how many packets went out.
///
/// Separated from the loop so the loop reads as a set of cases rather than as
/// the bodies of those cases.
async fn send_audio(
    writer: &tokio::sync::mpsc::Sender<Message>,
    side_channel: Option<&tokio::sync::mpsc::Sender<Vec<u8>>>,
    encoder: &mut AudioPacketEncoder,
    audio: Option<&mut crate::audio::AudioCaptureSession>,
    clock: &mut AudioTimeline,
    sent: &mut u64,
    stats_dropped: &mut u64,
) -> Result<(), StreamError> {
    let Some(session) = audio else {
        return Ok(());
    };
    let mut packets = session.drain_packets();
    if side_channel.is_none() {
        // Bounded before writing, not after. This drain used to write every packet
        // the capture side had accumulated, and that buffer holds two seconds of
        // stereo — up to a hundred packets, each one an await on the same
        // connection the video uses, ahead of video in the loop that picks what to
        // do next. A backlog therefore did not merely delay sound, it held the
        // writer while frames queued behind it. The Linux Pier has never had this
        // shape: it bounds the queue at eight and takes one packet at a time.
        let dropped = arcen_media::audio::trim_audio_backlog(
            &mut packets,
            arcen_media::audio::AUDIO_SEND_BACKLOG_PACKETS,
        );
        if dropped > 0 {
            *stats_dropped += dropped as u64;
            tracing::debug!(
                target: arcen_telemetry::names::target::MEDIA,
                dropped,
                "dropped audio too old to play in time rather than delay the picture behind it"
            );
        }
    }
    for packet in packets {
        let Some(payload) = encoder.encode(&packet, clock.next()) else {
            *stats_dropped += 1;
            continue;
        };
        // Never waits. A full priority lane means the wire is behind by more
        // audio than is worth playing; this packet is dropped and counted,
        // and the loop goes on reading input and taking frames.
        if let Some(channel) = side_channel {
            // The lane is sized for a burst (AUDIO_PRIORITY_SEND_BACKLOG_PACKETS),
            // so a full lane is a stalled writer, not jitter: drop and count
            // rather than let audio block video and input.
            match channel.try_send(payload) {
                Ok(()) => {}
                Err(_) => {
                    *stats_dropped += 1;
                    continue;
                }
            }
        } else {
            match writer.try_send(Message::Binary(payload)) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    *stats_dropped += 1;
                    continue;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    return Err(StreamError::PeerGone("writer stopped".to_owned()));
                }
            }
        }
        // Committed against the session's counter here, not accumulated
        // locally and returned at the end. A batch that sent three packets and
        // failed on the fourth used to report none of them, so the record of a
        // session that lost its client mid-drain understated what the client
        // had actually received.
        *sent += 1;
    }
    Ok(())
}

/// Starts the task that writes audio frames to the service's side channel.
///
/// Returns the sender audio is handed to, and the task, which ends when the
/// sender is dropped or the channel closes.
fn spawn_audio_channel(
    channel: Option<tokio::net::UnixStream>,
) -> (
    Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    Option<tokio::task::JoinHandle<()>>,
) {
    let Some(mut channel) = channel else {
        return (None, None);
    };
    let (sender, mut frames) = tokio::sync::mpsc::channel::<Vec<u8>>(
        arcen_media::audio::AUDIO_PRIORITY_SEND_BACKLOG_PACKETS,
    );
    let task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        while let Some(frame) = frames.recv().await {
            let mut buffer = Vec::with_capacity(4 + frame.len());
            buffer.extend_from_slice(&u32::try_from(frame.len()).unwrap_or(u32::MAX).to_be_bytes());
            buffer.extend_from_slice(&frame);
            if channel.write_all(&buffer).await.is_err() {
                return;
            }
        }
    });
    (Some(sender), Some(task))
}

/// The presentation clock for one session's audio.
///
/// Anchored to the wire clock once and then advanced by exactly one packet
/// duration, which is what the Linux Pier does. Stamping each packet with the
/// wall clock instead would carry capture and scheduling jitter into the
/// presentation times, and a player that spaces audio by its timestamps would
/// reproduce that jitter as audible unevenness — the packets are 20 ms of
/// sound whatever moment the host happened to drain them.
#[derive(Debug, Default)]
pub struct AudioTimeline {
    next: Option<u32>,
}

impl AudioTimeline {
    /// Returns the timestamp for the next packet.
    fn next(&mut self) -> u32 {
        let timestamp = match self.next {
            Some(previous) => {
                previous.wrapping_add(u32::from(arcen_media::audio::AUDIO_V1_FRAME_DURATION_MS))
            }
            None => arcen_protocol::wire::now_wire_timestamp_ms(),
        };
        self.next = Some(timestamp);
        timestamp
    }
}

/// Frames one PCM packet for the wire.
///
/// The samples are interleaved 16-bit, which is what the shared audio contract
/// calls `audio-v1`, and they are written little-endian because that is what
/// the packet body is defined as; the header's own fields are big-endian and
/// are written by the shared encoder rather than by hand here.
fn encode_audio_packet(samples: &[i16], timestamp_ms: u32) -> Vec<u8> {
    let header = arcen_protocol::wire::encode_audio_header(arcen_protocol::wire::AudioHeader {
        codec: arcen_protocol::wire::AudioCodec::Pcm,
        timestamp_ms,
    });
    let mut payload = Vec::with_capacity(header.len() + samples.len() * 2);
    payload.extend_from_slice(&header);
    for sample in samples {
        payload.extend_from_slice(&sample.to_le_bytes());
    }
    payload
}

/// How often the loop asks whether a health snapshot is owed.
///
/// Not the snapshot interval itself — `SnapshotCadence` owns that. This is only
/// how often the question is asked, and it must be well under the cadence so a
/// stalled session is reported near its due time rather than a cadence late.
const HEALTH_TICK: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a still desktop may go without sending anything.
///
/// One second, matching the Linux Pier. A Deck that has received nothing for
/// longer cannot tell a still desktop from a dead host.
const KEEPALIVE: Duration = Duration::from_secs(1);
/// How often the encoding rate is reconsidered.
const RATE_TICK: Duration = Duration::from_secs(1);
/// Longest the encoder sleeps on an idle desktop before checking what it owes.
const IDLE_WAKE: Duration = Duration::from_millis(100);

/// Returns the health-snapshot cadence for a session.
///
/// The interval is overridable so an end-to-end test can observe a real
/// snapshot without streaming for five seconds. A test that has to wait that
/// long gets skipped, and an emission nobody checks is an emission nobody
/// knows is broken.
fn snapshot_cadence() -> arcen_telemetry::SnapshotCadence {
    std::env::var("ARCEN_HEALTH_SNAPSHOT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or_else(
            arcen_telemetry::SnapshotCadence::new,
            arcen_telemetry::SnapshotCadence::every_secs,
        )
}

fn capture_frame_timeout() -> Duration {
    std::env::var(FRAME_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(FRAME_TIMEOUT, Duration::from_millis)
}

fn first_frame_timeout() -> Duration {
    std::env::var(FIRST_FRAME_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(FIRST_FRAME_TIMEOUT, Duration::from_millis)
}

pub fn write_timeout() -> Duration {
    std::env::var(WRITE_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(WRITE_TIMEOUT, Duration::from_millis)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureWait {
    FirstFrameStillPending,
    IdleAfterFirstFrame,
}

fn classify_capture_wait(
    error: CaptureError,
    has_frame: bool,
    first_frame_wait: Duration,
    first_frame_limit: Duration,
) -> Result<CaptureWait, StreamError> {
    match error {
        CaptureError::FrameTimeout if !has_frame && first_frame_wait >= first_frame_limit => {
            Err(StreamError::Capture(CaptureError::FirstFrameTimeout))
        }
        CaptureError::FrameTimeout if !has_frame => Ok(CaptureWait::FirstFrameStillPending),
        CaptureError::FrameTimeout => Ok(CaptureWait::IdleAfterFirstFrame),
        CaptureError::StreamEnded => Err(StreamError::Capture(CaptureError::StreamEnded)),
        error => Err(StreamError::Capture(error)),
    }
}

/// How many outbound bulk messages — video, clipboard — may wait for the wire.
///
/// Bounded, so a client that stops reading still applies backpressure instead
/// of growing this without limit. Two, because everything queued here is
/// latency: the frames are already encoded and cannot be dropped to catch up.
/// Eight was a quarter of a second of 1440p that audio had to wait behind.
const WRITER_QUEUE_DEPTH: usize = 2;
/// How many priority messages — audio, replies, cursor shapes — may wait.
///
/// Eight audio packets is the same 160 ms the shared audio backlog allows.
const PRIORITY_QUEUE_DEPTH: usize = 8;

/// Owns the sink and drains the queue, so the session never awaits a flush.
///
/// `SinkExt::send` is `poll_ready` then `start_send` then `poll_flush`, and
/// awaiting it inside the session loop meant the session stopped reading
/// input, stopped draining audio and stopped taking frames off the producer
/// queue for as long as the wire took. Measured on the lab at 2560x1440: 26.33
/// ms in send and 71.96 ms of frames waiting behind it, against 42 ms of
/// capture and 24 ms of encode — the two largest numbers in the session, both
/// caused by the session doing the writing itself.
///
/// The Linux Pier has never done this: it pushes into a bounded channel and a
/// writer owns the sink. This is that shape, as a future the same task drives
/// rather than a spawned task, because the sink borrows the socket and cannot
/// outlive it.
///
/// # Errors
///
/// Returns the first write failure, which ends the session.
async fn drive_writer<S>(
    sink: &mut S,
    priority: &mut tokio::sync::mpsc::Receiver<Message>,
    bulk: &mut tokio::sync::mpsc::Receiver<Message>,
) -> Result<(), StreamError>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    // Two lanes, the priority one always first: the Linux Pier's
    // `sender_loop` shape. Audio and replies are small and late audio is
    // audible; a picture waits one packet longer and nobody sees it. With one
    // FIFO, sound queued behind every frame ahead of it and arrived in bursts,
    // which the Deck heard as a queue swinging between trimmed and empty.
    let mut priority_open = true;
    let mut bulk_open = true;
    while priority_open || bulk_open {
        let message = tokio::select! {
            biased;
            message = priority.recv(), if priority_open => {
                if message.is_none() {
                    priority_open = false;
                }
                message
            }
            message = bulk.recv(), if bulk_open => {
                if message.is_none() {
                    bulk_open = false;
                }
                message
            }
        };
        if let Some(message) = message {
            send_message(sink, message).await?;
        }
    }
    Ok(())
}

/// Hands a message to the writer, or reports that the writer is gone.
///
/// Awaits queue space, never the wire. A full queue is the client not reading,
/// which is backpressure worth feeling; a closed queue is the writer having
/// failed, and the error it failed with is the one the session reports.
async fn enqueue(
    writer: &tokio::sync::mpsc::Sender<Message>,
    message: Message,
) -> Result<(), StreamError> {
    writer
        .send(message)
        .await
        .map_err(|_| StreamError::PeerGone("writer stopped".to_owned()))
}

async fn send_input_mode_results(
    writer: &tokio::sync::mpsc::Sender<Message>,
    results: &InputModeResults,
) -> Result<(), StreamError> {
    let cursor = serde_json::to_string(&results.cursor)
        .map_err(|error| StreamError::Protocol(format!("serialize cursor mode result: {error}")))?;
    enqueue(writer, Message::Text(cursor)).await?;
    let tablet = serde_json::to_string(&results.tablet)
        .map_err(|error| StreamError::Protocol(format!("serialize tablet mode result: {error}")))?;
    enqueue(writer, Message::Text(tablet)).await
}

/// Sends one message with an explicit write deadline.
///
/// # Errors
///
/// Returns [`StreamError::WriteTimeout`] when peer flow control prevents
/// progress, or [`StreamError::PeerGone`] when the framing layer fails.
pub async fn send_message_with_timeout<S>(
    sink: &mut S,
    message: Message,
    timeout: Duration,
) -> Result<(), StreamError>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    tokio::time::timeout(timeout, futures_util::SinkExt::send(sink, message))
        .await
        .map_err(|_| StreamError::WriteTimeout)?
        .map_err(|error| StreamError::PeerGone(error.to_string()))
}

/// Sends one post-handshake message under the stream write deadline.
///
/// # Errors
///
/// Returns [`StreamError::WriteTimeout`] when peer flow control prevents
/// progress, or [`StreamError::PeerGone`] when the framing layer fails.
pub async fn send_message<S>(sink: &mut S, message: Message) -> Result<(), StreamError>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_message_with_timeout(sink, message, write_timeout()).await
}

/// What an incoming message means for the stream loop.
enum Incoming {
    /// Handled; keep streaming.
    Continue,
    /// Transport path signal injected by a split service relay.
    PathSignal(arcen_telemetry::PathSignal),
    /// Handled, and the client is owed this answer.
    Reply(String),
    /// The client is gone.
    PeerLeft,
}

/// Answers a session-control message, if this one expects an answer.
///
/// Control messages used to fall through to the input parser, where they were
/// counted as unsupported input and dropped — a `health_ping` got no pong, so
/// a Deck displaying host health displayed nothing, and the count that would
/// have shown it is not in the stream summary either.
fn handle_control(
    text: &str,
    stats: &mut StreamStats,
    keyframe_requests: Option<&std::sync::atomic::AtomicU64>,
    last_keyframe_request: Option<&mut Instant>,
) -> Option<Incoming> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    match value.get("type").and_then(serde_json::Value::as_str)? {
        arcen_protocol::messages::HEALTH_PING => {
            let health_ping =
                serde_json::from_value::<arcen_protocol::messages::HealthPingMsg>(value).ok()?;
            let health_reply = arcen_protocol::messages::HealthPongMsg {
                msg_type: arcen_protocol::messages::HEALTH_PONG.to_owned(),
                ping_timestamp_ms: health_ping.timestamp_ms,
                sequence: health_ping.sequence,
                server_timestamp_ms: u64::from(arcen_protocol::wire::now_wire_timestamp_ms()),
                server_state: "streaming".to_owned(),
            };
            serde_json::to_string(&health_reply)
                .ok()
                .map(Incoming::Reply)
        }
        "path_signal" => {
            serde_json::from_value::<arcen_session::agent_relay::ServiceMessage>(value)
                .ok()
                .and_then(|message| match message {
                    arcen_session::agent_relay::ServiceMessage::PathSignal { signal, .. } => {
                        Some(Incoming::PathSignal(signal))
                    }
                    _ => None,
                })
        }
        arcen_protocol::messages::REQUEST_FULL_FRAME => {
            stats.full_frame_requests += 1;
            let request_keyframe = last_keyframe_request.is_none_or(|last_request| {
                let now = Instant::now();
                if now.duration_since(*last_request) < FULL_FRAME_KEYFRAME_GUARD {
                    return false;
                }
                *last_request = now;
                true
            });
            if request_keyframe && let Some(requests) = keyframe_requests {
                requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Some(Incoming::Continue)
        }
        _ => None,
    }
}

/// Applies one message from the client.
///
/// Text is either a clipboard offer or input; binary is dispatched by frame
/// type. A client asking for something this host has not built loses that
/// feature, not its desktop, so unsupported kinds are counted by name rather
/// than ending the session — counting is what makes the gap visible instead of
/// silent.
fn handle_incoming(
    message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    stats: &mut StreamStats,
    clipboard: Option<(
        &mut arcen_protocol::clipboard::ClipboardReassembler,
        &crate::clipboard_session::ClipboardWorker,
    )>,
    input: Option<&mut crate::input_session::InputSession>,
    keyframe_requests: Option<&std::sync::atomic::AtomicU64>,
    last_keyframe_request: Option<&mut Instant>,
) -> Result<Incoming, StreamError> {
    let (reassembler, worker) = match clipboard {
        Some((reassembler, worker)) => (Some(reassembler), Some(worker)),
        None => (None, None),
    };
    match message {
        Some(Ok(Message::Text(text))) => {
            let offer = read_clipboard_offer(&text);
            match (offer, reassembler) {
                // An offer opens a transfer. The shared reassembler owns what
                // follows: contiguity, sequence order, size and expiry are its
                // rules, not this host's.
                (Some(offer), Some(reassembler)) => {
                    if reassembler.begin(offer).is_err() {
                        stats.clipboard_rejected += 1;
                    }
                }
                // An offer arriving on a session that negotiated no clipboard
                // is refused rather than parsed. Counting it is what separates
                // "the client is confused" from "the host dropped it".
                (Some(_), None) => stats.clipboard_rejected += 1,
                (None, _) => {
                    if let Some(outcome) =
                        handle_control(&text, stats, keyframe_requests, last_keyframe_request)
                    {
                        return Ok(outcome);
                    }
                    if let Some(session) = input {
                        session
                            .apply(&text)
                            .map_err(|error| StreamError::Input(error.to_string()))?;
                    }
                }
            }
            Ok(Incoming::Continue)
        }
        Some(Ok(Message::Close(_))) | None => Ok(Incoming::PeerLeft),
        Some(Ok(Message::Binary(bytes))) => {
            let Some((reassembler, worker)) = reassembler.zip(worker) else {
                stats.clipboard_rejected += 1;
                return Ok(Incoming::Continue);
            };
            match dispatch_binary(&bytes, reassembler, worker) {
                BinaryOutcome::Handled => {}
                BinaryOutcome::Rejected => stats.clipboard_rejected += 1,
                BinaryOutcome::Unsupported(kind) => {
                    stats.unsupported_binary += 1;
                    stats.last_unsupported_binary = Some(kind);
                }
            }
            Ok(Incoming::Continue)
        }
        Some(Ok(_)) => Ok(Incoming::Continue),
        Some(Err(error)) => Err(StreamError::PeerGone(error.to_string())),
    }
}

/// Reads the clipboard and input counters and releases anything still held.
///
/// A key or button left down outlives the session on the physical machine, so
/// the release is unconditional rather than dependent on how the stream ended.
fn collect_peripheral_stats(
    stats: &mut StreamStats,
    clipboard: Option<&crate::clipboard_session::ClipboardWorker>,
    input: Option<&mut crate::input_session::InputSession>,
) {
    if let Some(clipboard) = clipboard {
        stats.clipboard = clipboard.stats();
    }
    if let Some(session) = input {
        stats.input = session.stats();
        let _ = session.release_all();
    }
}

/// The capture thread and the handles needed to read from and stop it.
#[derive(Clone)]
struct ProducerCancel {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ProducerCancel {
    fn new() -> Self {
        Self {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn token(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.cancelled)
    }

    fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for ProducerCancel {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Producer {
    frames: tokio::sync::mpsc::Receiver<Result<Produced, String>>,
    cancelled: ProducerCancel,
    handle: std::thread::JoinHandle<Result<(), StreamError>>,
    /// Frames `ScreenCaptureKit` shed because the encoder was behind.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Full-frame requests published by the async session and consumed by the
    /// blocking encoder thread.
    keyframe_requests: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frames not encoded because the compositor reported no change.
    suppressed: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The encoding rate the session wants, in bits per second; zero keeps
    /// the encoder's own.
    rate: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// The counters a producer publishes and the requests it reads.
///
/// Grouped because they travel together and always have: four separate
/// arguments threaded through two spawn paths is four chances to pass the
/// wrong one.
#[derive(Clone, Copy)]
struct ProducerSignals<'a> {
    /// Set when the client has gone and the producer should stop.
    cancelled: &'a std::sync::atomic::AtomicBool,
    /// Frames `ScreenCaptureKit` shed because the encoder was behind.
    dropped: &'a std::sync::atomic::AtomicU64,
    /// Full-frame requests published by the async session.
    keyframe_requests: &'a std::sync::atomic::AtomicU64,
    /// Frames not encoded because the compositor reported no change.
    suppressed: &'a std::sync::atomic::AtomicU64,
    /// The encoding rate the session wants; zero keeps the encoder's own.
    rate: &'a std::sync::atomic::AtomicU64,
}

/// Spawns the capture and encode thread.
///
/// Capture and encode run off the async runtime because `ScreenCaptureKit` hands
/// frames to a dispatch queue and `VideoToolbox` encode blocks; doing either on
/// the runtime would stall every other task, and the objects involved are not
/// `Send`, so they must not cross an await point.
///
/// Cancellation is explicit rather than implied by the channel closing,
/// because the producer can be parked inside a blocking capture wait or a
/// blocking send when the client disappears.
fn spawn_producer(
    capture: CaptureConfig,
    codec: EncoderCodec,
    motion_priority: arcen_media::video::MotionPriority,
    frame_budget: Option<u64>,
) -> Producer {
    // Two, not eight. Encoded frames cannot be dropped to catch up — a P-frame
    // whose reference never arrived is corruption until the next keyframe — so
    // anything queued here is latency the person feels and cannot be recovered
    // from. The place to absorb a rate mismatch is the raw handoff, where a
    // superseded frame costs nothing, and that is already latest-wins.
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<Result<Produced, String>>(2);
    let cancelled = ProducerCancel::new();
    let producer_cancel = cancelled.token();
    // Published by the capture thread and read by the session, so a shedding
    // host is visible in the record rather than inferred from a low rate.
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer_dropped = std::sync::Arc::clone(&dropped);
    let keyframe_requests = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer_keyframe_requests = std::sync::Arc::clone(&keyframe_requests);
    let suppressed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer_suppressed = std::sync::Arc::clone(&suppressed);
    let rate = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer_rate = std::sync::Arc::clone(&rate);
    let producer = std::thread::spawn(move || {
        produce(
            capture,
            codec,
            motion_priority,
            frame_budget,
            &frames_tx,
            ProducerSignals {
                cancelled: &producer_cancel,
                dropped: &producer_dropped,
                keyframe_requests: &producer_keyframe_requests,
                suppressed: &producer_suppressed,
                rate: &producer_rate,
            },
        )
    });
    Producer {
        frames: frames_rx,
        cancelled,
        handle: producer,
        dropped,
        keyframe_requests,
        suppressed,
        rate,
    }
}

/// Spawns the multi-display capture and encode thread.
fn spawn_multi_producer(
    monitors: Vec<RegionStreamPlan>,
    codec: EncoderCodec,
    motion_priority: arcen_media::video::MotionPriority,
    frame_budget: Option<u64>,
) -> Producer {
    // Two per monitor, for the reason given in `spawn_producer`.
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<Result<Produced, String>>(4);
    let cancelled = ProducerCancel::new();
    let producer_cancel = cancelled.token();
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer_dropped = std::sync::Arc::clone(&dropped);
    let keyframe_requests = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer_keyframe_requests = std::sync::Arc::clone(&keyframe_requests);
    let suppressed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer_suppressed = std::sync::Arc::clone(&suppressed);
    // Multi-monitor encoders keep their own rates for now; the session does
    // not drive this one.
    let rate = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer_rate = std::sync::Arc::clone(&rate);
    let producer = std::thread::spawn(move || {
        produce_multi(
            &monitors,
            codec,
            motion_priority,
            frame_budget,
            &frames_tx,
            ProducerSignals {
                cancelled: &producer_cancel,
                dropped: &producer_dropped,
                keyframe_requests: &producer_keyframe_requests,
                suppressed: &producer_suppressed,
                rate: &producer_rate,
            },
        )
    });
    Producer {
        frames: frames_rx,
        cancelled,
        handle: producer,
        dropped,
        keyframe_requests,
        suppressed,
        rate,
    }
}

/// Starts the pasteboard worker and the shared transfer reassembler.
///
/// A session without a pasteboard still streams; it just carries no clipboard,
/// which is better than refusing the desktop. The reassembler owns transfer
/// state — one transfer at a time, contiguous offsets, increasing sequence,
/// expiry of an abandoned offer — and re-deriving those rules per host is how
/// two hosts end up disagreeing about the same client.
fn start_clipboard(
    negotiation: Option<arcen_media::clipboard::ClipboardNegotiation>,
) -> Result<
    Option<(
        crate::clipboard_session::ClipboardWorker,
        arcen_protocol::clipboard::ClipboardReassembler,
    )>,
    StreamError,
> {
    // No negotiation means no pasteboard is opened at all. The host used to
    // start the worker unconditionally and let the policy refuse individual
    // payloads, which still read the local pasteboard on every poll for a user
    // who had switched the clipboard off.
    let Some(negotiation) = negotiation else {
        return Ok(None);
    };
    let worker = crate::clipboard_session::ClipboardWorker::start(negotiation, CLIPBOARD_POLL);
    let reassembler =
        arcen_protocol::clipboard::ClipboardReassembler::new(negotiation.policy().max_bytes)
            .map_err(|error| StreamError::Clipboard(format!("clipboard reassembler: {error:?}")))?;
    Ok(Some((worker, reassembler)))
}

/// Stops the capture thread and surfaces any failure it hit after its last
/// good frame.
///
/// The order matters. The producer can be blocked inside `blocking_send` on a
/// full channel, so it must be told to stop *and* the channel drained before
/// joining it — joining first would wait on a thread waiting on a receiver
/// nobody is reading. `join` also blocks, so it runs off the async runtime
/// rather than stalling every other task on this worker.
async fn shutdown_producer(
    cancelled: &ProducerCancel,
    frames_rx: &mut tokio::sync::mpsc::Receiver<Result<Produced, String>>,
    producer: std::thread::JoinHandle<Result<(), StreamError>>,
) -> Result<(), StreamError> {
    shutdown_producer_with_timeout(cancelled, frames_rx, producer, PRODUCER_JOIN_TIMEOUT).await
}

async fn shutdown_producer_with_timeout(
    cancelled: &ProducerCancel,
    frames_rx: &mut tokio::sync::mpsc::Receiver<Result<Produced, String>>,
    producer: std::thread::JoinHandle<Result<(), StreamError>>,
    timeout: Duration,
) -> Result<(), StreamError> {
    cancelled.cancel();
    frames_rx.close();
    while frames_rx.try_recv().is_ok() {}
    let joined = match crate::blocking::join_thread_with_timeout(
        "arcen-macos-producer-reaper",
        producer,
        timeout,
    )
    .await
    .map_err(|error| StreamError::Encode(format!("start capture reaper: {error}")))?
    {
        crate::blocking::JoinOutcome::Completed(joined) => joined,
        crate::blocking::JoinOutcome::TimedOut => {
            return Err(StreamError::Encode(format!(
                "capture thread did not stop within {timeout:?}"
            )));
        }
        crate::blocking::JoinOutcome::ReaperStopped => {
            return Err(StreamError::Encode(
                "capture reaper ended without a result".to_owned(),
            ));
        }
    };
    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(StreamError::Encode("capture thread panicked".to_owned())),
    }
}

/// The durations accumulated while streaming, summed across sent frames.
#[derive(Clone, Copy)]
struct Timings {
    total_frame: Duration,
    worst_frame: Duration,
    total_queue: Duration,
    total_send: Duration,
    total_capture_wait: Duration,
    total_encode: Duration,
}

/// Turns accumulated durations into the per-frame averages an operator reads.
///
/// Every mean is guarded on a non-zero frame count: a session that sent
/// nothing must report zero rather than a division by zero rendered as `NaN`,
/// which reads in a log like a measurement rather than an absence of one.
fn finalize_timings(stats: &mut StreamStats, elapsed: Duration, timings: Timings) {
    stats.elapsed_ms = elapsed.as_millis();
    let averages = arcen_telemetry::stage_averages(
        elapsed,
        stats.frames_sent,
        arcen_telemetry::StageTotals {
            frame: timings.total_frame,
            worst_frame: timings.worst_frame,
            capture_wait: timings.total_capture_wait,
            encode: timings.total_encode,
            send: timings.total_send,
        },
    );
    stats.sent_fps = averages.sent_fps;
    stats.mean_frame_ms = averages.mean_frame_ms;
    stats.max_frame_ms = averages.max_frame_ms;
    stats.mean_capture_wait_ms = averages.mean_capture_wait_ms;
    stats.mean_encode_ms = averages.mean_encode_ms;
    stats.mean_send_ms = averages.mean_send_ms;
    // Computed here rather than through the shared stage averages, which the
    // other hosts share and this measurement has not been proven useful to yet.
    stats.mean_queue_ms = if stats.frames_sent == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            timings.total_queue.as_secs_f64() * 1000.0 / stats.frames_sent as f64
        }
    };
}

/// How a measured rate compares with the rate this session promised.
///
/// Previously any nonzero rate reported `ok`, so a session serving four frames
/// a second against a sixty-frame contract — unusable, and the exact thing a
/// health snapshot exists to surface — was indistinguishable from a healthy
/// one. Only a total stall was visible, and by then the session was already
/// gone.
///
/// The thresholds are deliberately generous, because `ScreenCaptureKit` is
/// damage-driven: a still desktop legitimately produces almost no frames, and
/// calling that a fault would make the signal useless on the idle sessions
/// that make up most of a working day. `degraded` therefore means the host is
/// trying and failing to keep up, not that nobody moved the mouse.
fn health_verdict(fps_actual: u32, fps_target: u32) -> &'static str {
    if fps_target == 0 {
        return "ok";
    }
    if fps_actual == 0 {
        return "critical";
    }
    if fps_actual * 2 < fps_target {
        return "degraded";
    }
    "ok"
}

/// Emits one `HEALTH_SNAPSHOT` for a live session.
///
/// The schema declares which fields a health snapshot carries, and the encode
/// and frame timings are not among them, so they are not smuggled in under
/// invented names. They reach the record through the session summary instead,
/// where the schema has somewhere to put them. What belongs here is the
/// measured frame rate, which is the number that shows a stall while it is
/// still happening rather than after the session ends.
fn emit_health_snapshot(
    telemetry: &crate::observability::HostTelemetry,
    session_id: &arcen_telemetry::CorrelationId,
    fps_actual: u32,
    fps_target: u32,
    stats: &StreamStats,
) {
    if !telemetry.is_enabled() {
        return;
    }
    let health = health_verdict(fps_actual, fps_target);
    let Some(fields) = arcen_telemetry::lifecycle_fields::health_snapshot(
        health,
        Some(fps_actual),
        Some(fps_target),
    ) else {
        return;
    };
    telemetry.emit(
        arcen_telemetry::LifecycleEventKind::HealthSnapshot,
        &crate::observability::SessionScope::service(session_id.clone()),
        fields,
        arcen_telemetry::names::target::HEALTH,
        "health snapshot",
    );
    emit_health_diagnostics(fps_actual, stats);
}

fn emit_health_diagnostics(fps_actual: u32, stats: &StreamStats) {
    // The canonical HEALTH_SNAPSHOT schema has no field for per-stage timings
    // or input counters, and the schema is append-only and shared, so they are
    // not smuggled in under invented names. They ride the diagnostic channel
    // instead, which reaches the same sinks and keeps latency measurable
    // rather than asserted.
    //
    // Capture-wait, encode and send are reported separately because they fail
    // differently: a slow encoder and a slow network both show up as a low
    // frame rate, and only the split says which.
    tracing::info!(
        target: arcen_telemetry::names::target::MEDIA,
        fps_actual,
        frames_captured = stats.frames_captured,
        frames_dropped = stats.frames_dropped,
        frames_encoded = stats.frames_encoded,
        frames_sent = stats.frames_sent,
        frames_suppressed = stats.frames_suppressed,
        bytes_sent = stats.bytes_sent,
        mean_capture_wait_ms = stats.mean_capture_wait_ms,
        mean_frame_ms = stats.mean_frame_ms,
        mean_encode_ms = stats.mean_encode_ms,
        mean_send_ms = stats.mean_send_ms,
        mean_queue_ms = stats.mean_queue_ms,
        max_frame_ms = stats.max_frame_ms,
        "streaming",
    );
    // Pen throughput is reported whether or not a pen is in use: a zero here
    // during a tablet session is the evidence that samples are not arriving,
    // and it is only evidence if the line is printed when the count is zero.
    tracing::info!(
        target: arcen_telemetry::names::target::HID,
        input_applied = stats.input.applied,
        input_out_of_order = stats.input.out_of_order,
        pen_samples = stats.input.pen_samples,
        pen_proximity_edges = stats.input.pen_proximity_edges,
        pen_rejected = stats.input.pen_rejected,
        "input",
    );
}

/// Everything one streaming session needs besides its socket.
///
/// Grouped rather than passed as eight positional arguments, several of which
/// are `Option`s of unrelated things: at that width a call site can transpose
/// two of them and still compile.
#[derive(Debug)]
pub struct StreamSession<'a> {
    /// Which display, at what size and format.
    pub capture: CaptureConfig,
    /// Which encoder.
    pub codec: EncoderCodec,
    /// Shared detail/motion trade-off hook for this session.
    pub motion_priority: arcen_media::video::MotionPriority,
    /// Stop after this many frames, for probes and tests.
    pub frame_budget: Option<u64>,
    /// Where normalized input coordinates land.
    pub input_bounds: crate::input::DesktopBounds,
    /// Where records go.
    pub telemetry: crate::observability::HostTelemetry,
    /// Ties this session's records together.
    pub session_id: arcen_telemetry::CorrelationId,
    /// Host audio, when the session carries it.
    pub audio: Option<&'a mut crate::audio::AudioCaptureSession>,
    /// The clipboard this session negotiated, if any.
    ///
    /// `None` means no pasteboard is opened at all. Starting the worker and
    /// letting a policy refuse individual payloads still reads the local
    /// pasteboard on every poll, for a user who had switched clipboard off.
    pub clipboard: Option<arcen_media::clipboard::ClipboardNegotiation>,
    /// Who draws the pointer: the Deck locally, or the compositor into the
    /// picture. Only the local case wants a shape reported.
    pub cursor_mode: arcen_protocol::messages::CursorMode,
    /// Authoritative cursor/tablet negotiation results for this session.
    pub input_mode_results: InputModeResults,
    /// The service's audio side channel for this session, when the Deck
    /// accepts audio on its own priority stream. Audio sent here never waits
    /// behind a video frame on the session socket.
    pub audio_channel: Option<tokio::net::UnixStream>,
    /// How audio is encoded for the wire.
    pub audio_encoding: AudioEncoding,
    /// Direct QUIC connection for path sampling. Relayed agents receive the
    /// same signal as local control messages injected by the service relay.
    pub path_signal_connection: Option<quinn::Connection>,
}

/// How a session's audio is encoded for the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEncoding {
    /// 16-bit PCM, 1.5 Mbit/s for 48 kHz stereo.
    Pcm,
    /// Opus at the negotiated tier, through the shared encoder.
    Opus(arcen_media::audio::AudioBitrateTier),
}

impl AudioEncoding {
    /// The encoding a negotiated stream asks for. Anything that is not Opus is
    /// sent as PCM, which every Deck decodes.
    #[must_use]
    pub fn for_stream(stream: arcen_media::audio::ResolvedAudioStream) -> Self {
        if stream.codec == Some(arcen_protocol::wire::AudioCodec::Opus) {
            Self::Opus(stream.bitrate)
        } else {
            Self::Pcm
        }
    }
}

/// Turns captured 20 ms packets into wire audio frames.
struct AudioPacketEncoder {
    opus: Option<arcen_media::audio::OpusEncoder>,
    buffer: Vec<u8>,
}

impl AudioPacketEncoder {
    fn new(encoding: AudioEncoding) -> Self {
        let opus = match encoding {
            AudioEncoding::Pcm => None,
            AudioEncoding::Opus(tier) => match arcen_media::audio::OpusEncoder::new(tier) {
                Ok(encoder) => Some(encoder),
                Err(error) => {
                    tracing::warn!(
                        target: arcen_telemetry::names::target::MEDIA,
                        %error,
                        "no Opus encoder; audio is sent as PCM"
                    );
                    None
                }
            },
        };
        Self {
            opus,
            buffer: vec![0_u8; arcen_media::audio::MAX_OPUS_PACKET_BYTES],
        }
    }

    /// One wire frame for one packet, or `None` when encoding failed and the
    /// packet is better dropped than sent mislabelled.
    fn encode(&mut self, samples: &[i16], timestamp_ms: u32) -> Option<Vec<u8>> {
        let Some(opus) = self.opus.as_mut() else {
            return Some(encode_audio_packet(samples, timestamp_ms));
        };
        let length = opus.encode(samples, &mut self.buffer).ok()?;
        let header = arcen_protocol::wire::encode_audio_header(arcen_protocol::wire::AudioHeader {
            codec: arcen_protocol::wire::AudioCodec::Opus,
            timestamp_ms,
        });
        let mut payload = Vec::with_capacity(header.len() + length);
        payload.extend_from_slice(&header);
        payload.extend_from_slice(&self.buffer[..length]);
        Some(payload)
    }
}

/// One monitor's capture and region-frame identity.
#[derive(Debug, Clone, Copy)]
pub struct RegionStreamPlan {
    pub capture: CaptureConfig,
    pub monitor_id: arcen_media::SessionMonitorId,
    pub topology_generation: arcen_media::TopologyGeneration,
    pub stream_epoch: arcen_media::MediaStreamEpoch,
}

/// Everything a multi-monitor streaming session needs besides its socket.
#[derive(Debug)]
pub struct MultiStreamSession<'a> {
    pub monitors: Vec<RegionStreamPlan>,
    pub codec: EncoderCodec,
    pub motion_priority: arcen_media::video::MotionPriority,
    pub frame_budget: Option<u64>,
    pub input: crate::input_session::InputMode,
    pub telemetry: crate::observability::HostTelemetry,
    pub session_id: arcen_telemetry::CorrelationId,
    pub audio: Option<&'a mut crate::audio::AudioCaptureSession>,
    /// The clipboard this session negotiated, if any.
    pub clipboard: Option<arcen_media::clipboard::ClipboardNegotiation>,
    /// Who draws the pointer: the Deck locally, or the compositor into the
    /// picture. Only the local case wants a shape reported.
    pub cursor_mode: arcen_protocol::messages::CursorMode,
    /// Authoritative cursor/tablet negotiation results for this session.
    pub input_mode_results: InputModeResults,
    /// How audio is encoded for the wire.
    pub audio_encoding: AudioEncoding,
}

/// A stream that stopped before the client left cleanly.
///
/// Carries the statistics as well as the cause. A session that ran for
/// minutes and then lost its client has produced exactly the evidence someone
/// will want, and returning only the error threw all of it away: the frame
/// count reported as zero, and the latency summary never emitted at all. That
/// is backwards — an abrupt ending is the one most worth looking at.
#[derive(Debug)]
pub struct StreamEnded {
    /// What the session managed to do before it stopped.
    pub stats: StreamStats,
    /// Why it stopped.
    pub error: StreamError,
}

impl std::fmt::Display for StreamEnded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

/// Streams the desktop to a connected client until it leaves or the budget is
/// spent.
///
/// # Errors
///
/// Returns [`StreamEnded`] when capture, encode, or the socket fails. It
/// carries the statistics as well as the cause, so a session that streamed
/// for minutes before losing its client still reports what it did.
#[allow(clippy::too_many_lines)]
pub async fn stream(
    socket: &mut PierSocket,
    session: StreamSession<'_>,
) -> Result<StreamStats, Box<StreamEnded>> {
    let StreamSession {
        capture,
        codec,
        frame_budget,
        input_bounds,
        telemetry,
        session_id,
        mut audio,
        clipboard: clipboard_negotiation,
        cursor_mode: session_cursor_mode,
        input_mode_results,
        audio_channel,
        audio_encoding,
        path_signal_connection,
        motion_priority,
    } = session;
    let mut audio_encoder = AudioPacketEncoder::new(audio_encoding);
    let (side_audio, side_audio_writer) = spawn_audio_channel(audio_channel);
    let Producer {
        frames: mut frames_rx,
        cancelled,
        handle: producer,
        dropped: capture_dropped,
        keyframe_requests,
        suppressed: capture_suppressed,
        rate: encode_rate,
    } = spawn_producer(capture, codec, motion_priority, frame_budget);
    // What the path carries decides how fast to encode; see
    // `arcen_media::rate_control`. The ceiling is what this session was sized
    // for, the rate the encoder starts at.
    let width = u32::try_from(capture.width).unwrap_or(u32::MAX);
    let height = u32::try_from(capture.height).unwrap_or(u32::MAX);
    let fps = capture.fps;
    let chroma = capture.pixel_format.chroma();
    let depth = capture.pixel_format.bit_depth();
    let start_bps = u64::from(arcen_media::video::link_capped_average_bitrate_bps(
        width, height, fps, chroma, depth,
    ));
    let ceiling_bps = u64::from(arcen_media::video::average_bitrate_bps(
        width, height, fps, chroma, depth,
    ));
    let mut rate_controller = arcen_media::rate_control::RateController::new(
        arcen_media::rate_control::RateControlPolicy::for_bounds_and_priority(
            start_bps,
            ceiling_bps,
            motion_priority,
        ),
    );
    let mut path_state = arcen_telemetry::PathSignalState::default();
    let mut latest_path_signal: Option<arcen_media::rate_control::PathSignal> = None;
    let mut rate_tick = tokio::time::interval(RATE_TICK);
    let rate_control = std::env::var("ARCEN_RATE_CONTROL").as_deref() != Ok("0");
    let mut rate_mark = (0_u64, 0_u64, Duration::ZERO);

    // The rate this session promised, so a snapshot can say whether it is being
    // met rather than only whether anything arrived at all.
    let fps_target = capture.fps;
    let started = Instant::now();
    let mut cadence = snapshot_cadence();
    // Driven by a timer rather than by frame arrival. The check used to sit
    // after `send_frame`, so the one condition a health snapshot exists to
    // report — no frames at all — produced no snapshot either. A capture that
    // wedged went silent in exactly the way a healthy idle desktop does.
    //
    // The tick is deliberately finer than the cadence: `due` is the authority
    // on when a snapshot is owed, and asking it often costs a comparison.
    let mut health_tick = tokio::time::interval(HEALTH_TICK);
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stats = StreamStats::default();
    let mut total_queue = Duration::ZERO;
    let mut total_send = Duration::ZERO;
    let mut total_capture_wait = Duration::ZERO;
    let mut total_encode = Duration::ZERO;
    let mut total_frame = Duration::ZERO;
    let mut worst_frame = Duration::ZERO;

    // The fallible part runs inside its own scope so that whatever happens,
    // the timings are finalised and the summary is emitted below. Returning
    // early from here used to skip both, which meant the sessions that ended
    // badly — the ones worth investigating — were the only ones that left no
    // latency evidence and a frame count of zero.
    // Owned outside the fallible block so the counters survive every ending.
    // Declared inside, a `?` anywhere in the loop dropped them before anything
    // could be read, and `finish` then reported the zeroes it started with —
    // for exactly the sessions worth investigating.
    //
    // Ownership, not a shared borrow: `ClipboardWorker` holds an
    // `mpsc::Receiver`, which is `Send` but not `Sync`, so a `&` spanning an
    // await would make this future non-`Send` and unspawnable.
    let mut input = crate::input_session::InputSession::new(input_bounds).ok();
    // A failed start is carried in rather than raised here, so it still ends the
    // session through `finish` and is reported like every other stream failure.
    let (mut clipboard, clipboard_start) = match start_clipboard(clipboard_negotiation) {
        Ok(worker) => (worker, None),
        Err(error) => (None, Some(error)),
    };

    let outcome: Result<(), StreamError> = async {
        if let Some(error) = clipboard_start {
            return Err(error);
        }
        // Split so video can be written while input is read. Without this the
        // session could only do one at a time, and a client pressing a key
        // while frames flowed would be ignored until the stream paused.
        let (mut sink, mut incoming) = socket.split();
        // The writer owns the sink; the loop below owns a queue into it. See
        // `drive_writer` for why the session must not do its own flushing.
        let (writer, mut writer_queue) = tokio::sync::mpsc::channel::<Message>(WRITER_QUEUE_DEPTH);
        let (priority, mut priority_queue) =
            tokio::sync::mpsc::channel::<Message>(PRIORITY_QUEUE_DEPTH);
        let pump = drive_writer(&mut sink, &mut priority_queue, &mut writer_queue);
        tokio::pin!(pump);
        send_input_mode_results(&priority, &input_mode_results).await?;
        // A session without an event source still streams; it just carries no
        // input, which is better than refusing the desktop.
        // Only worth running when the Deck draws the pointer itself. In host
        // cursor mode the compositor already draws the real one into the
        // picture, and naming it as well would have the Deck draw a second.
        let cursor = if session_cursor_mode == arcen_protocol::messages::CursorMode::Local {
            crate::cursor_probe::CursorWatcher::start()
        } else {
            None
        };
        if cursor.is_none() && session_cursor_mode == arcen_protocol::messages::CursorMode::Local {
            tracing::warn!(
                target: arcen_telemetry::names::target::HID,
                "cannot read the desktop cursor; the Deck will draw a plain arrow"
            );
        }
        let mut cursor_tick = tokio::time::interval(CURSOR_POLL);
        let mut cursor_shapes_sent = 0_u64;
        let mut clipboard_tick = tokio::time::interval(CLIPBOARD_POLL);
        let mut clipboard_send_tick = tokio::time::interval(CLIPBOARD_SEND_TICK);
        let mut audio_tick = tokio::time::interval(AUDIO_POLL);
        let mut audio_clock = AudioTimeline::default();
        let mut clipboard_sender = ClipboardSender::new();
        let mut last_keyframe_request = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);

        // The writer runs concurrently with the whole loop, not as one arm
        // inside it. As an arm it was polled only between iterations, so the
        // moment an arm awaited queue space the one task that drains that
        // queue stopped being polled: the session waited for the writer and
        // the writer waited to be polled. A Deck that vanished mid-frame left
        // the host wedged — holding its capacity-one session lease, refusing
        // every later connection with "a session is already active" until the
        // agent was restarted. Measured on the lab: thirteen minutes and still
        // refusing.
        // A finished frame waiting for room in the video lane. Taken from the
        // encoder only when there is room, so the loop never blocks on video:
        // while the lane is full the encoder waits, and capture's latest-frame
        // slot replaces the picture it would have encoded next.
        let mut pending_frame: Option<Produced> = None;
        let streaming = async {
        loop {
            tokio::select! {
                biased;
                // Input first: a keystroke waiting behind a frame is a keystroke
                // the person feels as lag.
                message = incoming.next() => {
                    let clipboard_arg = clipboard
                        .as_mut()
                        .map(|(worker, reassembler)| (reassembler, &*worker));
                    match handle_incoming(
                        message,
                        &mut stats,
                        clipboard_arg,
                        input.as_mut(),
                        Some(&keyframe_requests),
                        Some(&mut last_keyframe_request),
                    )? {
                        Incoming::Continue => continue,
                        Incoming::PathSignal(signal) => {
                            latest_path_signal = Some(signal);
                            continue;
                        }
                        Incoming::Reply(text) => {
                            enqueue(&priority, Message::Text(text)).await?;
                            continue;
                        }
                        Incoming::PeerLeft => break,
                    }
                }
                _ = audio_tick.tick() => {
                    // Audio is drained on its own cadence rather than between
                    // frames: a desktop that stops changing stops producing
                    // frames, and audio that only moved when the picture did
                    // would drop out exactly when someone is listening to
                    // something on a still screen.
                    send_audio(
                        &priority,
                        side_audio.as_ref(),
                        &mut audio_encoder,
                        audio.as_deref_mut(),
                        &mut audio_clock,
                        &mut stats.audio_packets_sent,
                        &mut stats.audio_packets_dropped,
                    )
                    .await?;
                    continue;
                }
                permit = writer.reserve(), if pending_frame.is_some() => {
                    let permit = permit
                        .map_err(|_| StreamError::PeerGone("writer stopped".to_owned()))?;
                    if let Some(frame) = pending_frame.take() {
                        let accumulators = Accumulators {
                            total_queue: &mut total_queue,
                            total_send: &mut total_send,
                            total_capture_wait: &mut total_capture_wait,
                            total_encode: &mut total_encode,
                            total_frame: &mut total_frame,
                            worst_frame: &mut worst_frame,
                        };
                        send_frame(permit, frame, &mut stats, accumulators);
                    }
                    continue;
                }
                _ = health_tick.tick() => {
                    // A session that streams for an hour should leave an hour
                    // of evidence, not one summary at the end that a crash
                    // discards. The rate is measured over the window that
                    // actually elapsed, so a stall reads as a low number and a
                    // dead capture as zero, rather than as silence.
                    if let Some(fps_actual) = cadence.due(started.elapsed(), stats.frames_sent) {
                        // Refreshed here rather than only at the end, so a pen
                        // that stops arriving mid-session is visible while the
                        // session is still up.
                        if let Some(session) = input.as_ref() {
                            stats.input = session.stats();
                        }
                        stats.frames_dropped =
                            capture_dropped.load(std::sync::atomic::Ordering::Relaxed);
                        finalize_timings(
                            &mut stats,
                            started.elapsed(),
                            Timings {
                                total_frame,
                                worst_frame,
                                total_queue,
                                total_send,
                                total_capture_wait,
                                total_encode,
                            },
                        );
                        emit_health_snapshot(
                            &telemetry,
                            &session_id,
                            fps_actual,
                            fps_target,
                            &stats,
                        );
                    }
                    continue;
                }
                _ = rate_tick.tick() => {
                    let (bytes, frames, queued) = rate_mark;
                    let frames_now = stats.frames_sent.saturating_sub(frames);
                    let path = path_signal_connection
                        .as_ref()
                        .map(|connection| {
                            arcen_transport::observe_quinn_path_signal(
                                &mut path_state,
                                started.elapsed(),
                                connection,
                            )
                        })
                        .or(latest_path_signal);
                    let sample = arcen_media::rate_control::RateSample {
                        delivered_bytes: stats.bytes_sent.saturating_sub(bytes),
                        elapsed: RATE_TICK,
                        mean_frame_wait: total_queue
                            .saturating_sub(queued)
                            .checked_div(u32::try_from(frames_now).unwrap_or(u32::MAX))
                            .unwrap_or_default(),
                        frames: frames_now,
                        pipeline_count: 1,
                        path,
                    };
                    rate_mark = (stats.bytes_sent, stats.frames_sent, total_queue);
                    if rate_control && let Some(change) = rate_controller.observe(sample) {
                        encode_rate.store(change.target_bps, std::sync::atomic::Ordering::Relaxed);
                        tracing::info!(
                            target: arcen_telemetry::names::target::MEDIA,
                            previous_bps = change.previous_bps,
                            target_bps = change.target_bps,
                            reason = change.reason.token(),
                            delivered_bps = sample.delivered_bytes * 8,
                            rtt_ms = sample.path.map(|path| path.rtt().as_millis()),
                            queue_delay_ms = sample.path.map(|path| path.queue_delay().as_millis()),
                                loss_rate_permille = sample
                                    .path
                                    .map(|path| (path.loss_rate() * 1000.0).round() as u64),
                            mean_frame_wait_ms = sample.mean_frame_wait.as_secs_f64() * 1000.0,
                            "encoder rate follows the path"
                        );
                    }
                    continue;
                }
                _ = clipboard_tick.tick() => {
                    clipboard_sender.poll_latest(clipboard.as_ref().map(|(worker, _)| worker));
                    if clipboard_sender.has_work() {
                        clipboard_sender.send_one(&writer).await?;
                    }
                    continue;
                }
                ready = frames_rx.recv(), if pending_frame.is_none() && !cancelled.is_cancelled() => {
                    match ready {
                        Some(next) => {
                            pending_frame = Some(next.map_err(StreamError::Encode)?);
                            continue;
                        }
                        None => break,
                    }
                }
                _ = cursor_tick.tick(), if cursor.is_some() => {
                    // After frames: a pointer shape is worth a few
                    // milliseconds of delay and a picture is not.
                    if let Some(message) = cursor.as_ref().and_then(
                        crate::cursor_probe::CursorWatcher::take_changed,
                    ) && let Ok(text) = serde_json::to_string(&message) {
                        enqueue(&priority, Message::Text(text)).await?;
                        cursor_shapes_sent += 1;
                    }
                    continue;
                }
                _ = clipboard_send_tick.tick(), if clipboard_sender.has_work() => {
                    clipboard_sender.send_one(&writer).await?;
                    continue;
                }
            }
        }

        // Reported together, because separately they mislead. Images counts
        // what the desktop actually did; shapes counts what could be named and
        // sent. Images climbing while shapes stays at zero means the polling
        // works and the matching does not — which is a different bug from the
        // cursor never changing at all, and the two look identical otherwise.
        if let Some(cursor) = cursor.as_ref() {
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                cursor_images_seen = cursor.images_seen(),
                cursor_shapes_sent,
                "cursor shape reporting"
            );
        }
        Ok::<(), StreamError>(())
        };
        tokio::pin!(streaming);
        tokio::select! {
            biased;
            // The writer failing ends the session however healthy the loop
            // looks: everything the loop does is queue work for it.
            result = &mut pump => result,
            result = &mut streaming => result,
        }
    }
    .await;
    let cleanup = shutdown_producer(&cancelled, &mut frames_rx, producer).await;
    let outcome = match (outcome, cleanup) {
        (Ok(()), cleanup) => cleanup,
        (Err(error), _) => Err(error),
    };

    stats.frames_dropped = capture_dropped.load(std::sync::atomic::Ordering::Relaxed);
    stats.frames_suppressed = capture_suppressed.load(std::sync::atomic::Ordering::Relaxed);

    // On every ending, not only the clean one.
    collect_peripheral_stats(
        &mut stats,
        clipboard.as_ref().map(|(worker, _)| worker),
        input.as_mut(),
    );

    finish(
        stats,
        outcome,
        &telemetry,
        started.elapsed(),
        Timings {
            total_frame,
            worst_frame,
            total_queue,
            total_send,
            total_capture_wait,
            total_encode,
        },
    )
}

/// Streams every admitted monitor as region-video frames.
///
/// # Errors
///
/// Returns [`StreamEnded`] when capture, encode, or the socket fails. It
/// carries aggregate statistics across every monitor.
#[allow(clippy::too_many_lines)]
pub async fn stream_multi(
    socket: &mut PierSocket,
    session: MultiStreamSession<'_>,
) -> Result<StreamStats, Box<StreamEnded>> {
    let MultiStreamSession {
        monitors,
        codec,
        motion_priority,
        frame_budget,
        input,
        telemetry,
        session_id,
        mut audio,
        clipboard: clipboard_negotiation,
        cursor_mode: session_cursor_mode,
        input_mode_results,
        audio_encoding,
    } = session;
    let mut audio_encoder = AudioPacketEncoder::new(audio_encoding);
    // Counted before the plans are moved into the producer.
    let region_count = u32::try_from(monitors.len()).unwrap_or(1).max(1);
    let fps_target = monitors
        .first()
        .map_or(0, |monitor| monitor.capture.fps)
        .saturating_mul(region_count);
    let Producer {
        frames: mut frames_rx,
        cancelled,
        handle: producer,
        dropped: capture_dropped,
        keyframe_requests,
        suppressed: capture_suppressed,
        rate: _,
    } = spawn_multi_producer(monitors, codec, motion_priority, frame_budget);

    // Every monitor in a multi-display session is captured at the same rate,
    // and the aggregate `frames_sent` counts all of them, so the target a
    // snapshot is judged against is that rate times the number of regions.
    // Comparing an aggregate against a single display's target would report a
    // healthy two-screen session as running at double its contract.
    let started = Instant::now();
    let mut cadence = snapshot_cadence();
    // Driven by a timer rather than by frame arrival. The check used to sit
    // after `send_frame`, so the one condition a health snapshot exists to
    // report — no frames at all — produced no snapshot either. A capture that
    // wedged went silent in exactly the way a healthy idle desktop does.
    //
    // The tick is deliberately finer than the cadence: `due` is the authority
    // on when a snapshot is owed, and asking it often costs a comparison.
    let mut health_tick = tokio::time::interval(HEALTH_TICK);
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stats = StreamStats::default();
    let mut total_queue = Duration::ZERO;
    let mut total_send = Duration::ZERO;
    let mut total_capture_wait = Duration::ZERO;
    let mut total_encode = Duration::ZERO;
    let mut total_frame = Duration::ZERO;
    let mut worst_frame = Duration::ZERO;

    // Owned outside the fallible block so the counters survive every ending.
    // Declared inside, a `?` anywhere in the loop dropped them before anything
    // could be read, and `finish` then reported the zeroes it started with —
    // for exactly the sessions worth investigating.
    //
    // Ownership, not a shared borrow: `ClipboardWorker` holds an
    // `mpsc::Receiver`, which is `Send` but not `Sync`, so a `&` spanning an
    // await would make this future non-`Send` and unspawnable.
    let mut input = input.start().ok();
    // A failed start is carried in rather than raised here, so it still ends the
    // session through `finish` and is reported like every other stream failure.
    let (mut clipboard, clipboard_start) = match start_clipboard(clipboard_negotiation) {
        Ok(worker) => (worker, None),
        Err(error) => (None, Some(error)),
    };

    let outcome: Result<(), StreamError> = async {
        if let Some(error) = clipboard_start {
            return Err(error);
        }
        let (mut sink, mut incoming) = socket.split();
        // The writer owns the sink; the loop below owns a queue into it. See
        // `drive_writer` for why the session must not do its own flushing.
        let (writer, mut writer_queue) = tokio::sync::mpsc::channel::<Message>(WRITER_QUEUE_DEPTH);
        let (priority, mut priority_queue) =
            tokio::sync::mpsc::channel::<Message>(PRIORITY_QUEUE_DEPTH);
        let pump = drive_writer(&mut sink, &mut priority_queue, &mut writer_queue);
        tokio::pin!(pump);
        send_input_mode_results(&priority, &input_mode_results).await?;
        // Only worth running when the Deck draws the pointer itself. In host
        // cursor mode the compositor already draws the real one into the
        // picture, and naming it as well would have the Deck draw a second.
        let cursor = if session_cursor_mode == arcen_protocol::messages::CursorMode::Local {
            crate::cursor_probe::CursorWatcher::start()
        } else {
            None
        };
        if cursor.is_none() && session_cursor_mode == arcen_protocol::messages::CursorMode::Local {
            tracing::warn!(
                target: arcen_telemetry::names::target::HID,
                "cannot read the desktop cursor; the Deck will draw a plain arrow"
            );
        }
        let mut cursor_tick = tokio::time::interval(CURSOR_POLL);
        let mut cursor_shapes_sent = 0_u64;
        let mut clipboard_tick = tokio::time::interval(CLIPBOARD_POLL);
        let mut clipboard_send_tick = tokio::time::interval(CLIPBOARD_SEND_TICK);
        let mut audio_tick = tokio::time::interval(AUDIO_POLL);
        let mut audio_clock = AudioTimeline::default();
        let mut clipboard_sender = ClipboardSender::new();
        let mut last_keyframe_request = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);

        // Concurrent with the whole loop, not an arm inside it: see the
        // single-display path above for the deadlock that arrangement caused.
        // A finished frame waiting for room in the video lane. Taken from the
        // encoder only when there is room, so the loop never blocks on video:
        // while the lane is full the encoder waits, and capture's latest-frame
        // slot replaces the picture it would have encoded next.
        let mut pending_frame: Option<Produced> = None;
        let streaming = async {
        loop {
            tokio::select! {
                biased;
                message = incoming.next() => {
                    let clipboard_arg = clipboard
                        .as_mut()
                        .map(|(worker, reassembler)| (reassembler, &*worker));
                    match handle_incoming(
                        message,
                        &mut stats,
                        clipboard_arg,
                        input.as_mut(),
                        Some(&keyframe_requests),
                        Some(&mut last_keyframe_request),
                    )? {
                        Incoming::Continue => continue,
                        Incoming::PathSignal(_) => continue,
                        Incoming::Reply(text) => {
                            enqueue(&priority, Message::Text(text)).await?;
                            continue;
                        }
                        Incoming::PeerLeft => break,
                    }
                }
                _ = audio_tick.tick() => {
                    send_audio(
                        &priority,
                        None,
                        &mut audio_encoder,
                        audio.as_deref_mut(),
                        &mut audio_clock,
                        &mut stats.audio_packets_sent,
                        &mut stats.audio_packets_dropped,
                    )
                    .await?;
                    continue;
                }
                permit = writer.reserve(), if pending_frame.is_some() => {
                    let permit = permit
                        .map_err(|_| StreamError::PeerGone("writer stopped".to_owned()))?;
                    if let Some(frame) = pending_frame.take() {
                        let accumulators = Accumulators {
                            total_queue: &mut total_queue,
                            total_send: &mut total_send,
                            total_capture_wait: &mut total_capture_wait,
                            total_encode: &mut total_encode,
                            total_frame: &mut total_frame,
                            worst_frame: &mut worst_frame,
                        };
                        send_frame(permit, frame, &mut stats, accumulators);
                    }
                    continue;
                }
                _ = health_tick.tick() => {
                    // A session that streams for an hour should leave an hour
                    // of evidence, not one summary at the end that a crash
                    // discards. The rate is measured over the window that
                    // actually elapsed, so a stall reads as a low number and a
                    // dead capture as zero, rather than as silence.
                    if let Some(fps_actual) = cadence.due(started.elapsed(), stats.frames_sent) {
                        // Refreshed here rather than only at the end, so a pen
                        // that stops arriving mid-session is visible while the
                        // session is still up.
                        if let Some(session) = input.as_ref() {
                            stats.input = session.stats();
                        }
                        stats.frames_dropped =
                            capture_dropped.load(std::sync::atomic::Ordering::Relaxed);
                        finalize_timings(
                            &mut stats,
                            started.elapsed(),
                            Timings {
                                total_frame,
                                worst_frame,
                                total_queue,
                                total_send,
                                total_capture_wait,
                                total_encode,
                            },
                        );
                        emit_health_snapshot(
                            &telemetry,
                            &session_id,
                            fps_actual,
                            fps_target,
                            &stats,
                        );
                    }
                    continue;
                }
                _ = clipboard_tick.tick() => {
                    clipboard_sender.poll_latest(clipboard.as_ref().map(|(worker, _)| worker));
                    if clipboard_sender.has_work() {
                        clipboard_sender.send_one(&writer).await?;
                    }
                    continue;
                }
                ready = frames_rx.recv(), if pending_frame.is_none() && !cancelled.is_cancelled() => {
                    match ready {
                        Some(next) => {
                            pending_frame = Some(next.map_err(StreamError::Encode)?);
                            continue;
                        }
                        None => break,
                    }
                }
                _ = cursor_tick.tick(), if cursor.is_some() => {
                    // After frames: a pointer shape is worth a few
                    // milliseconds of delay and a picture is not.
                    if let Some(message) = cursor.as_ref().and_then(
                        crate::cursor_probe::CursorWatcher::take_changed,
                    ) && let Ok(text) = serde_json::to_string(&message) {
                        enqueue(&priority, Message::Text(text)).await?;
                        cursor_shapes_sent += 1;
                    }
                    continue;
                }
                _ = clipboard_send_tick.tick(), if clipboard_sender.has_work() => {
                    clipboard_sender.send_one(&writer).await?;
                    continue;
                }
            }
        }

        // Reported together, because separately they mislead. Images counts
        // what the desktop actually did; shapes counts what could be named and
        // sent. Images climbing while shapes stays at zero means the polling
        // works and the matching does not — which is a different bug from the
        // cursor never changing at all, and the two look identical otherwise.
        if let Some(cursor) = cursor.as_ref() {
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                cursor_images_seen = cursor.images_seen(),
                cursor_shapes_sent,
                "cursor shape reporting"
            );
        }
        Ok::<(), StreamError>(())
        };
        tokio::pin!(streaming);
        tokio::select! {
            biased;
            result = &mut pump => result,
            result = &mut streaming => result,
        }
    }
    .await;
    let cleanup = shutdown_producer(&cancelled, &mut frames_rx, producer).await;
    let outcome = match (outcome, cleanup) {
        (Ok(()), cleanup) => cleanup,
        (Err(error), _) => Err(error),
    };

    stats.frames_dropped = capture_dropped.load(std::sync::atomic::Ordering::Relaxed);
    stats.frames_suppressed = capture_suppressed.load(std::sync::atomic::Ordering::Relaxed);

    // On every ending, not only the clean one.
    collect_peripheral_stats(
        &mut stats,
        clipboard.as_ref().map(|(worker, _)| worker),
        input.as_mut(),
    );

    finish(
        stats,
        outcome,
        &telemetry,
        started.elapsed(),
        Timings {
            total_frame,
            worst_frame,
            total_queue,
            total_send,
            total_capture_wait,
            total_encode,
        },
    )
}

/// The running timing totals one frame contributes to.
struct Accumulators<'a> {
    total_send: &'a mut Duration,
    /// How long finished frames waited for the writer to take them.
    total_queue: &'a mut Duration,
    total_capture_wait: &'a mut Duration,
    total_encode: &'a mut Duration,
    total_frame: &'a mut Duration,
    worst_frame: &'a mut Duration,
}

/// Writes one encoded frame to the client and accounts for it.
///
/// The counters move only after the write succeeds, so a frame that never
/// reached the client is never counted as sent.
fn send_frame(
    permit: tokio::sync::mpsc::Permit<'_, Message>,
    frame: Produced,
    stats: &mut StreamStats,
    totals: Accumulators<'_>,
) {
    stats.frames_captured = frame.captured;
    stats.frames_encoded = frame.encoded;
    if frame.keyframe {
        stats.keyframes += 1;
    }
    let send_started = Instant::now();
    *totals.total_queue += send_started.saturating_duration_since(frame.queued_at);
    let sent_bytes = frame.payload.len() as u64;
    permit.send(Message::Binary(frame.payload));
    *totals.total_send += send_started.elapsed();
    *totals.total_capture_wait += frame.capture_wait;
    *totals.total_encode += frame.encode_took;
    *totals.total_frame += frame.took;
    *totals.worst_frame = (*totals.worst_frame).max(frame.took);
    stats.frames_sent += 1;
    stats.bytes_sent += sent_bytes;
}

/// Finalises the timings, emits the summary, and reports what the session did.
///
/// Runs on every ending, not just the clean one. The summary used to be
/// skipped whenever the stream returned early, so the sessions that ended
/// badly were the only ones that left no latency evidence behind.
fn finish(
    mut stats: StreamStats,
    outcome: Result<(), StreamError>,
    telemetry: &crate::observability::HostTelemetry,
    elapsed: Duration,
    timings: Timings,
) -> Result<StreamStats, Box<StreamEnded>> {
    finalize_timings(&mut stats, elapsed, timings);
    emit_stream_summary(telemetry, &stats);
    match outcome {
        Ok(()) => Ok(stats),
        Err(error) => Err(Box::new(StreamEnded { stats, error })),
    }
}

/// Reports the per-stage timings for a finished session.
///
/// These ride the diagnostic channel because the canonical `SESSION_END`
/// schema declares no field for them, and that schema is shared and
/// append-only. `SESSION_END` still carries `frames_sent`, which is what says
/// whether a picture arrived at all; this says where the time went.
fn emit_stream_summary(telemetry: &crate::observability::HostTelemetry, stats: &StreamStats) {
    if !telemetry.is_enabled() {
        return;
    }
    tracing::info!(
        target: arcen_telemetry::names::target::MEDIA,
        frames_captured = stats.frames_captured,
        frames_encoded = stats.frames_encoded,
        frames_sent = stats.frames_sent,
        frames_suppressed = stats.frames_suppressed,
        bytes_sent = stats.bytes_sent,
        audio_packets_sent = stats.audio_packets_sent,
        audio_packets_dropped = stats.audio_packets_dropped,
        sent_fps = stats.sent_fps,
        mean_capture_wait_ms = stats.mean_capture_wait_ms,
        mean_frame_ms = stats.mean_frame_ms,
        mean_encode_ms = stats.mean_encode_ms,
        mean_send_ms = stats.mean_send_ms,
        mean_queue_ms = stats.mean_queue_ms,
        mean_frame_ms = stats.mean_frame_ms,
        max_frame_ms = stats.max_frame_ms,
        "stream summary",
    );
    tracing::info!(
        target: arcen_telemetry::names::target::HID,
        input_applied = stats.input.applied,
        input_out_of_order = stats.input.out_of_order,
        input_malformed = stats.input.malformed,
        input_unmapped_keys = stats.input.unmapped_keys,
        input_scroll_events = stats.input.scroll_events,
        pen_samples = stats.input.pen_samples,
        pen_proximity_edges = stats.input.pen_proximity_edges,
        pen_rejected = stats.input.pen_rejected,
        "input summary",
    );
}

/// One finished frame, ready for the wire.
struct Produced {
    /// When encoding finished and the frame joined the queue to the writer.
    ///
    /// Carried so the time a frame spends waiting for the writer can be told
    /// apart from the time it spends in flight. The Deck reports the age of a
    /// frame when it presents it; without this, a host cannot say whether that
    /// age was accumulated on its own side or beyond it.
    queued_at: Instant,
    payload: Vec<u8>,
    keyframe: bool,
    captured: u64,
    encoded: u64,
    took: Duration,
    capture_wait: Duration,
    encode_took: Duration,
}

struct EncodeCounters<'a> {
    captured: &'a mut u64,
    encoded: &'a mut u64,
    encoded_keyframe_request: &'a mut u64,
}

fn encode_single_frame(
    encoder: &mut Encoder,
    frame: &CapturedFrame,
    capture_wait: Duration,
    counters: &mut EncodeCounters<'_>,
    codec: EncoderCodec,
    pixel_format: WireShape,
    keyframe_requests: &std::sync::atomic::AtomicU64,
) -> Result<Option<Produced>, StreamError> {
    let frame_started = Instant::now();
    let encode_started = Instant::now();
    let requested_keyframe = keyframe_requests.load(std::sync::atomic::Ordering::Relaxed);
    let force_keyframe = requested_keyframe != *counters.encoded_keyframe_request;
    let Some(unit) = encoder
        .encode_with_keyframe_request(frame, force_keyframe)
        .map_err(|error| StreamError::Encode(error.to_string()))?
    else {
        return Ok(None);
    };
    if force_keyframe {
        *counters.encoded_keyframe_request = requested_keyframe;
    }
    let encode_took = encode_started.elapsed();
    *counters.encoded += 1;

    let timestamp_ms = arcen_protocol::wire::now_wire_timestamp_ms();
    let mut payload = header_for(&unit, codec, pixel_format, timestamp_ms);
    payload.extend_from_slice(&unit.bytes);

    Ok(Some(Produced {
        queued_at: Instant::now(),
        payload,
        keyframe: unit.keyframe,
        captured: *counters.captured,
        encoded: *counters.encoded,
        took: frame_started.elapsed(),
        capture_wait,
        encode_took,
    }))
}

/// Captures and encodes until the budget is met, the session is cancelled, or
/// the consumer hangs up.
///
/// An idle desktop does not end this loop: `ScreenCaptureKit` delivers frames
/// when the screen changes, so a still screen is a normal state rather than a
/// reason to disconnect.
#[allow(clippy::too_many_lines)]
/// One captured surface with the time the capture thread waited for it.
struct StagedFrame {
    frame: CapturedFrame,
    capture_wait: Duration,
}

// SAFETY: a staged frame owns an `IOSurface` and the `CMSampleBuffer` holding
// its pool lease. Both are CoreFoundation-style objects whose retain and
// release are atomic and whose contents carry no thread affinity; Apple's own
// capture pipelines hand sample buffers from a capture queue to an encoding
// thread, which is exactly this transfer. `objc2` withholds `Send` from every
// CF type by default because Objective-C objects in general may be
// thread-affine, not because `CMSampleBuffer` is.
//
// Arcen already depends on this being true: `ScreenCaptureKit` builds each
// `CapturedFrame` on its dispatch queue and a different thread receives it. The
// `block2` closure that does so is not required to be `Send`, so that transfer
// has always been unchecked rather than proven — this states the property
// where it can be read instead of leaving it implied.
//
// Only `Send` is asserted. A frame moves between threads one owner at a time;
// `Sync` would permit two threads to hold the same surface at once, which
// nothing here needs and the pool accounting could not survive.
#[allow(unsafe_code)]
unsafe impl Send for StagedFrame {}

/// The one-slot handoff between the capture thread and the encode thread.
///
/// Latest-wins on purpose. A live desktop has no use for a frame that has been
/// superseded, and queueing them costs twice: the person sees older pixels, and
/// every queued frame keeps one `ScreenCaptureKit` pool surface checked out.
/// That second cost is not theoretical — a probe that held sixty surfaces from
/// the eight-surface pool stopped receiving frames altogether, and
/// `dropped_frames` stayed at zero throughout, because starving the pool stops
/// delivery upstream of the counter that exists to notice loss.
struct LatestFrame {
    slot: Mutex<Option<StagedFrame>>,
    ready: Condvar,
    closed: std::sync::atomic::AtomicBool,
    superseded: std::sync::atomic::AtomicU64,
}

impl LatestFrame {
    fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            ready: Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            superseded: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Publishes a frame, replacing any the encoder has not taken yet.
    ///
    /// Returns false once the consumer is gone, which is how the capture thread
    /// learns to stop.
    fn publish(&self, staged: StagedFrame) -> bool {
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.replace(staged).is_some() {
            self.superseded
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        drop(slot);
        self.ready.notify_one();
        true
    }

    /// Takes the newest frame, waiting up to `timeout` for one to appear.
    ///
    /// `None` means the wait expired with nothing published, which is an idle
    /// desktop rather than a fault.
    fn take(&self, timeout: Duration) -> Option<StagedFrame> {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(staged) = slot.take() {
            return Some(staged);
        }
        let (mut slot, _) = self
            .ready
            .wait_timeout(slot, timeout)
            .unwrap_or_else(PoisonError::into_inner);
        slot.take()
    }

    /// Releases the held surface and wakes both sides so they can finish.
    fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        // Dropped here rather than at scope exit so the pool gets its surface
        // back before the capture session is torn down.
        slot.take();
        drop(slot);
        self.ready.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn superseded(&self) -> u64 {
        self.superseded.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Runs capture and encode as two overlapping stages.
///
/// They used to be one loop: wait for a frame, encode it, block until the
/// writer took it, then go back and wait for the next one. Every stage paid for
/// the one before it, so the session ran at the rate of their sum rather than
/// of the slowest. The recorded means from a real session say what that cost —
/// 24.10 ms waiting, 15.39 ms encoding, 25.84 ms sending, which predicts
/// 1000/65.33 = 15.31 fps against 15.34 fps actually measured. Stages that add
/// up like that are not merely slow, they are serialised, and no amount of
/// making any one of them faster changes the shape.
///
/// Split, the session runs at the slowest stage instead of the sum. The same
/// measured costs then predict roughly 30 fps rather than 15, and the capture
/// thread returns to `ScreenCaptureKit` immediately instead of holding a pool
/// surface for the length of an encode and a network write.
fn produce(
    capture: CaptureConfig,
    codec: EncoderCodec,
    motion_priority: arcen_media::video::MotionPriority,
    frame_budget: Option<u64>,
    frames: &tokio::sync::mpsc::Sender<Result<Produced, String>>,
    signals: ProducerSignals<'_>,
) -> Result<(), StreamError> {
    let ProducerSignals {
        cancelled,
        dropped,
        keyframe_requests,
        suppressed,
        rate,
    } = signals;
    let session = CaptureSession::start(capture).map_err(StreamError::Capture)?;

    let latest = LatestFrame::new();
    let outcome = std::thread::scope(|scope| {
        let encode_side = scope.spawn(|| {
            let result = encode_loop(
                &latest,
                EncodeLoopContext {
                    capture,
                    codec,
                    motion_priority,
                    frame_budget,
                    frames,
                    cancelled,
                    keyframe_requests,
                    suppressed,
                    rate,
                },
            );
            // Whatever ended the encode side, the capture side must stop
            // waiting for a reader that is no longer there.
            latest.close();
            result
        });
        let capture_result = capture_loop(&session, &latest, cancelled, dropped);
        latest.close();
        let encode_result = encode_side
            .join()
            .unwrap_or_else(|_| Err(StreamError::Encode("encode thread panicked".to_owned())));
        capture_result.and(encode_result)
    });

    session.stop();
    // The source's own rate, reported every session rather than only when it
    // looks wrong. `mean_capture_wait_ms` says how long the consumer waited;
    // this says how often the source offered. When the two agree, the
    // ceiling is ScreenCaptureKit's and no amount of consumer tuning moves it.
    let cadence = session.source_cadence();
    tracing::info!(
        target: arcen_telemetry::names::target::MEDIA,
        source_callbacks = cadence.callbacks,
        mean_source_interval_ms = cadence.mean_interval_ms,
        max_source_interval_ms = cadence.max_interval_ms,
        superseded = latest.superseded(),
        "capture source cadence",
    );
    if latest.superseded() > 0 {
        tracing::debug!(
            target: "arcen::media",
            superseded = latest.superseded(),
            "captured frames replaced by a newer one before the encoder took them"
        );
    }
    outcome
}

/// Reports which encoder `VideoToolbox` actually gave this session.
///
/// Read rather than assumed. Hardware encoding has been allowed by default
/// since macOS 10.15, so neither asking for it nor staying silent proves
/// anything about what was selected; only the read-back does. A host that
/// cannot tell a hardware session from a software one will describe both the
/// same way, and the first thing anyone asks about a slow stream is which one
/// it was.
fn report_encoder_backend(encoder: &Encoder) {
    match encoder.uses_hardware_acceleration() {
        Some(true) => tracing::info!(
            target: "arcen::media",
            encoder_backend = "hardware",
            "VideoToolbox selected a hardware encoder"
        ),
        Some(false) => tracing::warn!(
            target: "arcen::media",
            encoder_backend = "software",
            "VideoToolbox selected a software encoder; expect lower capacity"
        ),
        None => tracing::warn!(
            target: "arcen::media",
            encoder_backend = "unknown",
            "VideoToolbox did not report which encoder it selected"
        ),
    }
}

/// Pulls frames from `ScreenCaptureKit` and hands the newest to the encoder.
///
/// Deliberately does nothing else. Every millisecond spent here is a
/// millisecond a pool surface stays checked out, and a starved pool stops
/// delivery rather than reporting it.
///
/// # Errors
///
/// Returns [`StreamError::Capture`] when the capture session fails, as opposed
/// to merely producing no frame because the desktop is idle.
/// How many published frames between capture-cadence reports.
///
/// About ten seconds at the rates this host currently achieves: often enough
/// to see a session change behaviour, rare enough not to crowd the log.
const CADENCE_REPORT_FRAMES: u64 = 150;

fn capture_loop(
    session: &CaptureSession,
    latest: &LatestFrame,
    cancelled: &std::sync::atomic::AtomicBool,
    dropped: &std::sync::atomic::AtomicU64,
) -> Result<(), StreamError> {
    let frame_timeout = capture_frame_timeout();
    let first_frame_limit = first_frame_timeout();
    let first_frame_started = Instant::now();
    let mut seen_frame = false;
    let mut idle_waits = 0_u64;
    let mut published = 0_u64;

    loop {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) || latest.is_closed() {
            break;
        }
        let wait_started = Instant::now();
        match session.next_frame(frame_timeout) {
            Ok(frame) => {
                seen_frame = true;
                // Republished each turn rather than only at the end, so a host
                // that is shedding right now says so in its next health
                // snapshot instead of in a summary nobody sees until it is over.
                dropped.store(
                    session.dropped_frames(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                published += 1;
                // Reported while the session runs rather than at teardown,
                // because teardown is exactly when this is least reliable: the
                // producer is joined with a timeout and abandoned if it misses
                // it, so an end-of-run summary is the one record a wedged
                // capture will never write.
                if published.is_multiple_of(CADENCE_REPORT_FRAMES) {
                    let cadence = session.source_cadence();
                    tracing::info!(
                        target: arcen_telemetry::names::target::MEDIA,
                        source_callbacks = cadence.callbacks,
                        mean_source_interval_ms = cadence.mean_interval_ms,
                        max_source_interval_ms = cadence.max_interval_ms,
                        mean_callback_work_ms = cadence.mean_work_ms,
                        max_callback_work_ms = cadence.max_work_ms,
                        mean_arrival_age_ms = cadence.mean_arrival_age_ms,
                        max_arrival_age_ms = cadence.max_arrival_age_ms,
                        superseded = latest.superseded(),
                        "capture source cadence",
                    );
                }
                if !latest.publish(StagedFrame {
                    frame,
                    capture_wait: wait_started.elapsed(),
                }) {
                    break;
                }
            }
            Err(error) => match classify_capture_wait(
                error,
                seen_frame,
                first_frame_started.elapsed(),
                first_frame_limit,
            )? {
                CaptureWait::FirstFrameStillPending | CaptureWait::IdleAfterFirstFrame => {
                    idle_waits += 1;
                }
            },
        }
    }

    if idle_waits > 0 {
        tracing::debug!(
            target: "arcen::media",
            idle_waits,
            "capture waits that produced no frame, which is an idle desktop rather than a fault"
        );
    }
    Ok(())
}

/// What the encode stage needs that is not the encoder or the frame source.
#[derive(Clone, Copy)]
struct EncodeLoopContext<'a> {
    capture: CaptureConfig,
    codec: EncoderCodec,
    motion_priority: arcen_media::video::MotionPriority,
    frame_budget: Option<u64>,
    frames: &'a tokio::sync::mpsc::Sender<Result<Produced, String>>,
    cancelled: &'a std::sync::atomic::AtomicBool,
    keyframe_requests: &'a std::sync::atomic::AtomicU64,
    suppressed: &'a std::sync::atomic::AtomicU64,
    rate: &'a std::sync::atomic::AtomicU64,
}

/// Whether the encode stage has nothing left to do.
///
/// A budget reached, a cancelled session and a client that has gone all end the
/// same way, and naming them together keeps the loop about frames.
fn stream_is_finished(
    frame_budget: Option<u64>,
    sent: u64,
    cancelled: &std::sync::atomic::AtomicBool,
    frames: &tokio::sync::mpsc::Sender<Result<Produced, String>>,
) -> bool {
    frame_budget.is_some_and(|budget| sent >= budget)
        || cancelled.load(std::sync::atomic::Ordering::Relaxed)
        || frames.is_closed()
}

/// Serves a full-frame request from the last picture, if one is owed.
///
/// A Deck that cannot decode what it is being sent asks for a full frame, and a
/// still desktop is exactly when nothing else will produce one. Returns `None`
/// when nothing is owed or there is no picture to serve it from, both of which
/// are ordinary.
///
/// # Errors
///
/// Returns [`StreamError::Encode`] when the encoder rejects the frame.
fn encode_pending_recovery_point(
    encoder: &mut Encoder,
    last_frame: Option<&CapturedFrame>,
    counters: &mut EncodeCounters<'_>,
    codec: EncoderCodec,
    pixel_format: WireShape,
    keyframe_requests: &std::sync::atomic::AtomicU64,
) -> Result<Option<Produced>, StreamError> {
    let requested = keyframe_requests.load(std::sync::atomic::Ordering::Relaxed);
    if requested == *counters.encoded_keyframe_request {
        return Ok(None);
    }
    let Some(frame) = last_frame else {
        return Ok(None);
    };
    encode_single_frame(
        encoder,
        frame,
        Duration::ZERO,
        counters,
        codec,
        pixel_format,
        keyframe_requests,
    )
}

/// How each frame's damage was classified, counted over a session.
///
/// `frames_suppressed` stayed at zero on a desktop nobody was touching, and
/// zero is consistent with two opposite causes: the compositor naming changed
/// regions on every frame, or this host never learning what changed and
/// conservatively treating every frame as fully damaged. They need opposite
/// fixes, so they are counted apart.
#[derive(Debug, Default, Clone, Copy)]
struct DamageCensus {
    /// No usable damage metadata; the whole frame is assumed changed.
    unknown: u64,
    /// The compositor positively said nothing changed.
    still: u64,
    /// The compositor named changed regions.
    named: u64,
    /// Rectangles named, summed, so an average area is available.
    rects: u64,
    /// Damaged pixels named, summed over frames.
    damaged_pixels: u64,
    /// Frame pixels, summed over the same frames, so the two divide.
    frame_pixels: u64,
}

impl DamageCensus {
    /// Classifies one frame's reported damage.
    ///
    /// Deliberately separate from `observe_damage`, which owns the policy.
    /// This only counts, so the policy keeps one caller and one meaning.
    fn observe(&mut self, reported: &crate::capture::FrameDamage, frame_pixels: u64) {
        self.frame_pixels += frame_pixels;
        match reported {
            crate::capture::FrameDamage::Unknown => self.unknown += 1,
            crate::capture::FrameDamage::Rects(rects) if rects.is_empty() => self.still += 1,
            crate::capture::FrameDamage::Rects(rects) => {
                self.named += 1;
                self.rects += rects.len() as u64;
                self.damaged_pixels += rects
                    .iter()
                    .map(|rect| u64::from(rect.width) * u64::from(rect.height))
                    .sum::<u64>();
            }
        }
    }
}

/// Folds one frame's reported damage into the accumulated damage and cadence.
///
/// Separated from the loop so the policy can be tested without a screen. The
/// three cases are genuinely different and collapsing any two of them is a
/// bug someone will have to find from a bandwidth graph:
///
/// - being told nothing is not being told nothing changed, so an unknown marks
///   the whole frame and counts as activity;
/// - an empty rectangle list is the compositor positively saying the desktop is
///   still, which owes a keepalive but not a picture;
/// - anything else is activity covering the blocks it names.
fn observe_damage(
    reported: &crate::capture::FrameDamage,
    damage: &mut arcen_keel::ExternalDamage,
    cadence: &mut arcen_keel::IdleCadence,
    width: usize,
    height: usize,
) {
    match reported {
        crate::capture::FrameDamage::Unknown => {
            damage.mark_rect(arcen_keel::PixelRect {
                x: 0,
                y: 0,
                width,
                height,
            });
            cadence.note_frame();
        }
        crate::capture::FrameDamage::Rects(rects) if rects.is_empty() => {
            cadence.note_unchanged_frame();
        }
        crate::capture::FrameDamage::Rects(rects) => {
            for rect in rects {
                damage.mark_rect(arcen_keel::PixelRect {
                    x: rect.x as usize,
                    y: rect.y as usize,
                    width: rect.width as usize,
                    height: rect.height as usize,
                });
            }
            cadence.note_frame();
        }
    }
}

fn detail_frame_interval(max_fps: u32, start_bps: u64, target_bps: u64) -> Duration {
    Duration::from_secs_f64(
        1.0 / f64::from(
            arcen_media::rate_control::detail_framerate(max_fps, start_bps, target_bps).max(1),
        ),
    )
}

/// Encodes the newest captured frame and hands it to the writer.
///
/// Blocking here is now safe in a way it was not before: this thread blocking
/// on an encode or on a full writer queue no longer stops capture, so the pool
/// keeps turning over and the frame this eventually encodes is a recent one.
///
/// The encoder is built here rather than handed in because a `VideoToolbox`
/// session is pinned to nothing but must not be moved across threads by us
/// either; owning it for the life of this loop keeps that question closed.
///
/// # Errors
///
/// Returns [`StreamError::Encode`] when the session cannot be created or a
/// frame cannot be encoded.
// One frame's life from staged to sent, including the decision not to send it.
// Splitting it further would scatter that sequence across functions that each
// need most of the same state, which is harder to follow than the length.
#[allow(clippy::too_many_lines)]
fn encode_loop(latest: &LatestFrame, context: EncodeLoopContext<'_>) -> Result<(), StreamError> {
    let EncodeLoopContext {
        capture,
        codec,
        frame_budget,
        frames,
        cancelled,
        keyframe_requests,
        suppressed,
        rate,
        motion_priority,
    } = context;
    let mut applied_rate = 0_u64;
    let start_bps = u64::from(arcen_media::video::link_capped_average_bitrate_bps(
        u32::try_from(capture.width).unwrap_or(u32::MAX),
        u32::try_from(capture.height).unwrap_or(u32::MAX),
        capture.fps,
        capture.pixel_format.chroma(),
        capture.pixel_format.bit_depth(),
    ));
    let mut detail_interval = Duration::from_secs_f64(1.0 / f64::from(capture.fps.max(1)));
    let mut encoder = Encoder::new(
        EncoderConfig::realtime_for(
            i32::try_from(capture.width).unwrap_or(1920),
            i32::try_from(capture.height).unwrap_or(1080),
            codec,
            capture.fps,
            capture.pixel_format.chroma(),
            capture.pixel_format.bit_depth(),
        )
        .with_motion_priority(motion_priority)
        .with_colour(crate::encode::colour_for_capture(&capture))
        .with_diagnostic_bitrate_override(),
    )
    .map_err(|error| StreamError::Encode(error.to_string()))?;
    report_encoder_backend(&encoder);
    let hdr_white = hdr_white_stage_for(&capture);
    let mut hdr_white_stats = HdrWhiteStats::default();
    let idle_timeout = capture_frame_timeout();
    // Damage-driven scheduling, the same shape the Linux Pier uses. A desktop
    // nobody is touching should cost a keepalive, not a stream: the compositor
    // already says what it redrew, and re-encoding an unchanged picture thirty
    // times a second spends a link's whole budget on a still image.
    let mut damage =
        arcen_keel::ExternalDamage::new(capture.width, capture.height).map_err(|error| {
            StreamError::Encode(format!(
                "damage tracking rejected the capture size: {error}"
            ))
        })?;
    let mut cadence = arcen_keel::IdleCadence::new(KEEPALIVE);
    let mut census = DamageCensus::default();
    let mut last_emit = Instant::now();
    let mut suppressed_here = 0_u64;
    let mut capture_count = 0_u64;
    let mut encode_count = 0_u64;
    let mut sent = 0_u64;
    let mut encoded_keyframe_request = 0_u64;
    let mut last_frame: Option<CapturedFrame> = None;

    loop {
        if stream_is_finished(frame_budget, sent, cancelled, frames) {
            break;
        }
        // Between frames, never inside one: VideoToolbox takes a new average
        // rate for the frames that follow.
        let wanted = rate.load(std::sync::atomic::Ordering::Relaxed);
        if wanted != 0 && wanted != applied_rate {
            if motion_priority == arcen_media::video::MotionPriority::Detail {
                detail_interval = detail_frame_interval(capture.fps, start_bps, wanted);
            }
            match encoder.set_average_bitrate(wanted) {
                Ok(()) => applied_rate = wanted,
                Err(error) => {
                    tracing::warn!(
                        target: arcen_telemetry::names::target::MEDIA,
                        %error,
                        "encoder refused a new rate"
                    );
                    applied_rate = wanted;
                }
            }
        }
        // Short waits, so an idle desktop still wakes to serve what it owes.
        // ScreenCaptureKit delivers nothing while the screen is still, so a
        // wait as long as the capture timeout meant a still desktop sent no
        // keepalive at all — the Deck showed its stall overlay over a healthy
        // session — and a recovery point asked for on a still screen went out
        // up to two seconds late.
        let Some(staged) = latest.take(idle_timeout.min(IDLE_WAKE)) else {
            // Nothing published within the wait: an idle desktop. It owes a
            // recovery point somebody asked for, and a keepalive once a second.
            let mut counters = EncodeCounters {
                captured: &mut capture_count,
                encoded: &mut encode_count,
                encoded_keyframe_request: &mut encoded_keyframe_request,
            };
            let mut produced = encode_pending_recovery_point(
                &mut encoder,
                last_frame.as_ref(),
                &mut counters,
                codec,
                WireShape::of(&capture),
                keyframe_requests,
            )?;
            if produced.is_none() && last_emit.elapsed() >= KEEPALIVE {
                if let Some(frame) = last_frame.as_ref() {
                    produced = encode_single_frame(
                        &mut encoder,
                        frame,
                        Duration::ZERO,
                        &mut counters,
                        codec,
                        WireShape::of(&capture),
                        keyframe_requests,
                    )?;
                }
            }
            let Some(produced) = produced else {
                continue;
            };
            if frames.blocking_send(Ok(produced)).is_err() {
                break;
            }
            sent += 1;
            last_emit = Instant::now();
            continue;
        };

        capture_count += 1;
        let capture_wait = staged.capture_wait;
        observe_damage(
            &staged.frame.damage,
            &mut damage,
            &mut cadence,
            capture.width,
            capture.height,
        );
        census.observe(
            &staged.frame.damage,
            (capture.width as u64) * (capture.height as u64),
        );
        if capture_count.is_multiple_of(CADENCE_REPORT_FRAMES) {
            tracing::info!(
                target: arcen_telemetry::names::target::MEDIA,
                damage_unknown = census.unknown,
                damage_still = census.still,
                damage_named = census.named,
                damage_rects = census.rects,
                damage_area_pct = if census.frame_pixels == 0 {
                    0.0
                } else {
                    100.0 * census.damaged_pixels as f64 / census.frame_pixels as f64
                },
                frames_suppressed = suppressed_here,
                "capture damage census",
            );
        }
        let idr_pending = keyframe_requests.load(std::sync::atomic::Ordering::Relaxed)
            != encoded_keyframe_request;
        if motion_priority == arcen_media::video::MotionPriority::Detail
            && !idr_pending
            && last_emit.elapsed() < detail_interval
        {
            suppressed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            suppressed_here += 1;
            continue;
        }
        let decision = cadence.decision(idr_pending, last_emit.elapsed());
        // Kept whatever the decision, because the damage it carries has already
        // been accumulated and a later frame will need this picture to serve a
        // recovery point from.
        last_frame = Some(whiten(
            hdr_white.as_ref(),
            staged.frame,
            &mut hdr_white_stats,
        ));
        // What the HDR stream actually carries, every five seconds or so: an
        // HDR application on the host shows here as highlights above the
        // 203-nit white every ordinary window sits at.
        if hdr_white.is_some() && capture_count.is_multiple_of(150) {
            if let Some((p99, max)) = last_frame
                .as_ref()
                .and_then(crate::capture::ten_bit_luma_census)
            {
                tracing::info!(
                    target: arcen_telemetry::names::target::MEDIA,
                    luma_p99 = p99,
                    luma_max = max,
                    p99_nits = crate::capture::pq_code_to_nits(p99),
                    max_nits = crate::capture::pq_code_to_nits(max),
                    white_stage_mean_ms = hdr_white_stats.mean_ms(),
                    white_stage_failures = hdr_white_stats.failures,
                    "HDR luminance census"
                );
            }
        }
        let Some(mode) = decision else {
            suppressed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            suppressed_here += 1;
            continue;
        };
        let Some(frame) = last_frame.as_ref() else {
            continue;
        };
        let Some(produced) = encode_single_frame(
            &mut encoder,
            frame,
            capture_wait,
            &mut EncodeCounters {
                captured: &mut capture_count,
                encoded: &mut encode_count,
                encoded_keyframe_request: &mut encoded_keyframe_request,
            },
            codec,
            WireShape::of(&capture),
            keyframe_requests,
        )?
        else {
            continue;
        };
        if frames.blocking_send(Ok(produced)).is_err() {
            // The client went away; stopping is correct, not an error.
            break;
        }
        sent += 1;
        // Committed only once the frame is actually on its way. Clearing the
        // accumulated damage when the decision was made instead would lose
        // every region a failed send was carrying.
        cadence.on_submitted();
        damage.reset();
        last_emit = Instant::now();
        if mode != arcen_keel::EmitMode::Activity {
            tracing::debug!(
                target: "arcen::media",
                reason = mode.name(),
                "sent a frame the desktop did not ask for"
            );
        }
    }

    if suppressed_here > 0 {
        tracing::debug!(
            target: "arcen::media",
            suppressed = suppressed_here,
            "frames not encoded because the compositor reported no change"
        );
    }
    Ok(())
}

struct RegionEncoder {
    plan: RegionStreamPlan,
    encoder: Encoder,
    captured: u64,
    encoded: u64,
    encoded_keyframe_request: u64,
    last_frame: Option<CapturedFrame>,
    /// This monitor's HDR white stage, when its capture is HDR.
    hdr_white: Option<crate::hdr_white::PqWhiteStage>,
    hdr_white_stats: HdrWhiteStats,
}

/// The HDR white stage for a capture, or `None` for SDR and when one cannot
/// be built — in which case the stream stays valid PQ, only dimmer, and the
/// reason is logged.
///
/// HDR frames arrive with SDR white where macOS puts it; the stream carries it
/// where the shared PQ contract does.
fn hdr_white_stage_for(capture: &CaptureConfig) -> Option<crate::hdr_white::PqWhiteStage> {
    if capture.dynamic_range == crate::capture::CaptureDynamicRange::Sdr {
        return None;
    }
    match crate::hdr_white::PqWhiteStage::new(
        capture.width,
        capture.height,
        arcen_media::video::pq_white::MACOS_CAPTURE_WHITE_NITS,
    ) {
        Ok(stage) => {
            tracing::info!(
                target: arcen_telemetry::names::target::MEDIA,
                display_id = capture.display_id,
                gain = stage.gain(),
                target_white_nits = arcen_media::video::pq_white::GRAPHICS_WHITE_NITS,
                "HDR white stage ready"
            );
            Some(stage)
        }
        Err(error) => {
            tracing::warn!(
                target: arcen_telemetry::names::target::MEDIA,
                display_id = capture.display_id,
                %error,
                "no HDR white stage; SDR content stays at the capture's white"
            );
            None
        }
    }
}

/// Runs `frame` through `stage`, or passes it on as captured when there is
/// none or it fails. A failed frame is still valid PQ; the count is logged at
/// powers of two so a persistent fault is visible without flooding.
fn whiten(
    stage: Option<&crate::hdr_white::PqWhiteStage>,
    frame: CapturedFrame,
    stats: &mut HdrWhiteStats,
) -> CapturedFrame {
    let Some(stage) = stage else {
        return frame;
    };
    let started = Instant::now();
    let result = stage.apply(&frame);
    stats.frames += 1;
    stats.elapsed += started.elapsed();
    match result {
        Ok(converted) => converted,
        Err(error) => {
            stats.failures += 1;
            if stats.failures.is_power_of_two() {
                tracing::warn!(
                    target: arcen_telemetry::names::target::MEDIA,
                    %error,
                    failures = stats.failures,
                    "HDR white stage failed; sending the frame as captured"
                );
            }
            frame
        }
    }
}

/// What the HDR white stage has cost so far.
#[derive(Debug, Default, Clone, Copy)]
struct HdrWhiteStats {
    frames: u64,
    failures: u64,
    elapsed: Duration,
}

impl HdrWhiteStats {
    /// Mean milliseconds per frame the stage ran on.
    fn mean_ms(self) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }
        // Frame counts stay far inside `f64`'s exact range.
        #[allow(clippy::cast_precision_loss)]
        let frames = self.frames as f64;
        self.elapsed.as_secs_f64() * 1000.0 / frames
    }
}

fn make_region_encoders(
    monitors: &[RegionStreamPlan],
    codec: EncoderCodec,
    motion_priority: arcen_media::video::MotionPriority,
) -> Result<Vec<RegionEncoder>, StreamError> {
    monitors
        .iter()
        .copied()
        .map(|plan| {
            let encoder = Encoder::new(
                EncoderConfig::realtime_for(
                    i32::try_from(plan.capture.width).unwrap_or(1920),
                    i32::try_from(plan.capture.height).unwrap_or(1080),
                    codec,
                    plan.capture.fps,
                    plan.capture.pixel_format.chroma(),
                    plan.capture.pixel_format.bit_depth(),
                )
                .with_motion_priority(motion_priority)
                .with_colour(crate::encode::colour_for_capture(&plan.capture)),
            )
            .map_err(|error| StreamError::Encode(error.to_string()))?;
            Ok(RegionEncoder {
                plan,
                encoder,
                captured: 0,
                encoded: 0,
                encoded_keyframe_request: 0,
                last_frame: None,
                hdr_white: hdr_white_stage_for(&plan.capture),
                hdr_white_stats: HdrWhiteStats::default(),
            })
        })
        .collect()
}

/// Captures and encodes every monitor in one blocking producer thread.
#[allow(clippy::too_many_lines)]
fn produce_multi(
    monitors: &[RegionStreamPlan],
    codec: EncoderCodec,
    motion_priority: arcen_media::video::MotionPriority,
    frame_budget: Option<u64>,
    frames: &tokio::sync::mpsc::Sender<Result<Produced, String>>,
    signals: ProducerSignals<'_>,
) -> Result<(), StreamError> {
    let ProducerSignals {
        cancelled,
        dropped,
        keyframe_requests,
        suppressed: _,
        rate: _,
    } = signals;
    let capture = crate::multi_capture::MultiDisplayCapture::start_configured(
        monitors.iter().map(|monitor| monitor.capture).collect(),
    )
    .map_err(|error| StreamError::Capture(CaptureError::StartFailed(error.to_string())))?;
    let mut encoders = make_region_encoders(monitors, codec, motion_priority)?;
    let mut sent = 0_u64;
    let mut total_captured = 0_u64;
    let mut total_encoded = 0_u64;
    let mut idle_waits = 0_u64;
    let first_frame_limit = first_frame_timeout();
    let first_frame_started = Instant::now();
    let frame_timeout = std::env::var(FRAME_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(Duration::from_millis(16), Duration::from_millis);

    loop {
        if frame_budget.is_some_and(|budget| sent >= budget)
            || cancelled.load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        dropped.store(
            capture.dropped_frames(),
            std::sync::atomic::Ordering::Relaxed,
        );
        let mut sent_any = false;
        for monitor in capture.monitors() {
            let index = usize::from(monitor.monitor_index);
            let Some(encoder) = encoders.get_mut(index) else {
                continue;
            };
            let wait_started = Instant::now();
            let frame = match monitor.next_frame(frame_timeout) {
                Ok(frame) => frame,
                Err(error) => match classify_capture_wait(
                    error,
                    encoder.last_frame.is_some(),
                    first_frame_started.elapsed(),
                    first_frame_limit,
                )? {
                    CaptureWait::FirstFrameStillPending => {
                        idle_waits += 1;
                        if frames.is_closed() {
                            return Ok(());
                        }
                        continue;
                    }
                    CaptureWait::IdleAfterFirstFrame => {
                        idle_waits += 1;
                        if frames.is_closed() {
                            return Ok(());
                        }
                        let requested_keyframe =
                            keyframe_requests.load(std::sync::atomic::Ordering::Relaxed);
                        let Some(frame) = encoder.last_frame.as_ref() else {
                            continue;
                        };
                        if requested_keyframe == encoder.encoded_keyframe_request {
                            continue;
                        }
                        let frame_started = Instant::now();
                        let capture_wait = wait_started.elapsed();
                        let encode_started = Instant::now();
                        let Some(unit) = encoder
                            .encoder
                            .encode_with_keyframe_request(frame, true)
                            .map_err(|error| StreamError::Encode(error.to_string()))?
                        else {
                            continue;
                        };
                        encoder.encoded_keyframe_request = requested_keyframe;
                        let encode_took = encode_started.elapsed();
                        encoder.encoded += 1;
                        total_encoded += 1;
                        let timestamp_ms = arcen_protocol::wire::now_wire_timestamp_ms();
                        let mut payload = region_header_for(
                            &unit,
                            codec,
                            WireShape::of(&encoder.plan.capture),
                            timestamp_ms,
                            encoder.plan.monitor_id,
                            encoder.plan.topology_generation,
                            encoder.plan.stream_epoch,
                        );
                        payload.extend_from_slice(&unit.bytes);
                        let produced = Produced {
                            queued_at: Instant::now(),
                            payload,
                            keyframe: unit.keyframe,
                            captured: total_captured,
                            encoded: total_encoded,
                            took: frame_started.elapsed(),
                            capture_wait,
                            encode_took,
                        };
                        if frames.blocking_send(Ok(produced)).is_err() {
                            return Ok(());
                        }
                        sent += 1;
                        sent_any = true;
                        continue;
                    }
                },
            };
            let capture_wait = wait_started.elapsed();
            let frame_started = Instant::now();
            encoder.captured += 1;
            total_captured += 1;
            let encode_started = Instant::now();
            let requested_keyframe = keyframe_requests.load(std::sync::atomic::Ordering::Relaxed);
            let force_keyframe = requested_keyframe != encoder.encoded_keyframe_request;
            encoder.last_frame = Some(whiten(
                encoder.hdr_white.as_ref(),
                frame,
                &mut encoder.hdr_white_stats,
            ));
            let Some(frame) = encoder.last_frame.as_ref() else {
                continue;
            };
            let Some(unit) = encoder
                .encoder
                .encode_with_keyframe_request(frame, force_keyframe)
                .map_err(|error| StreamError::Encode(error.to_string()))?
            else {
                continue;
            };
            if force_keyframe {
                encoder.encoded_keyframe_request = requested_keyframe;
            }
            let encode_took = encode_started.elapsed();
            encoder.encoded += 1;
            total_encoded += 1;
            let timestamp_ms = arcen_protocol::wire::now_wire_timestamp_ms();
            let mut payload = region_header_for(
                &unit,
                codec,
                WireShape::of(&encoder.plan.capture),
                timestamp_ms,
                encoder.plan.monitor_id,
                encoder.plan.topology_generation,
                encoder.plan.stream_epoch,
            );
            payload.extend_from_slice(&unit.bytes);
            let produced = Produced {
                queued_at: Instant::now(),
                payload,
                keyframe: unit.keyframe,
                captured: total_captured,
                encoded: total_encoded,
                took: frame_started.elapsed(),
                capture_wait,
                encode_took,
            };
            if frames.blocking_send(Ok(produced)).is_err() {
                return Ok(());
            }
            sent += 1;
            sent_any = true;
        }
        if !sent_any {
            std::thread::yield_now();
        }
    }
    capture.stop();
    if idle_waits > 0 {
        tracing::debug!(
            target: "arcen::media",
            idle_waits,
            "multi-display capture waits that produced no frame"
        );
    }
    Ok(())
}

/// What happened to a binary message from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryOutcome {
    /// The frame was understood and acted on.
    Handled,
    /// The frame was a clipboard chunk this host refused.
    Rejected,
    /// The frame names a capability this host has not built yet.
    Unsupported(UnsupportedBinary),
}

/// A binary frame kind the macOS Pier does not yet carry.
///
/// These are named rather than lumped together, because "the client sent
/// something we ignored" is not a diagnosis. Microphone audio and a USB
/// transfer fail for entirely different reasons and are fixed by different
/// work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedBinary {
    /// Upstream audio from the client's microphone.
    Microphone,
    /// A raw HID frame.
    RawHid,
    /// A USB transfer for a bridged device.
    UsbBridge,
    /// Video, which only ever travels host to client.
    Video,
    /// A frame type this build does not recognise at all.
    Unknown,
}

/// Routes one binary message from the client to whatever owns it.
///
/// The first byte is a [`FrameType`]. Treating every binary message as a
/// clipboard payload — which this host did previously — means a microphone
/// frame or a USB transfer is written to the pasteboard, and it means the
/// real Deck's framed clipboard chunks are written including their header.
fn dispatch_binary(
    bytes: &[u8],
    reassembler: &mut arcen_protocol::clipboard::ClipboardReassembler,
    clipboard: &crate::clipboard_session::ClipboardWorker,
) -> BinaryOutcome {
    use arcen_protocol::FrameType;

    let Some(kind) = bytes
        .first()
        .copied()
        .and_then(|byte| FrameType::try_from(byte).ok())
    else {
        return BinaryOutcome::Unsupported(UnsupportedBinary::Unknown);
    };

    match kind {
        FrameType::Clipboard => {
            let Ok((header, payload)) = arcen_protocol::decode_clipboard_chunk(bytes) else {
                reassembler.abort();
                return BinaryOutcome::Rejected;
            };
            match reassembler.push(header, payload) {
                Ok(Some(mut completed)) => {
                    let sequence = completed.sequence;
                    let completed_kind = completed.kind;
                    clipboard.apply_remote(sequence, completed_kind, completed.take_bytes());
                    BinaryOutcome::Handled
                }
                Ok(None) => BinaryOutcome::Handled,
                Err(_) => {
                    // A transfer that broke its own rules is abandoned whole.
                    // Keeping the partial bytes would let a later chunk
                    // complete a payload nobody offered.
                    reassembler.abort();
                    BinaryOutcome::Rejected
                }
            }
        }
        FrameType::AudioUpstream => BinaryOutcome::Unsupported(UnsupportedBinary::Microphone),
        FrameType::HidDeviceAdded | FrameType::HidDeviceRemoved | FrameType::HidReport => {
            BinaryOutcome::Unsupported(UnsupportedBinary::RawHid)
        }
        FrameType::UsbBridgeUrbSubmit
        | FrameType::UsbBridgeUrbCancel
        | FrameType::UsbBridgeUrbComplete => {
            BinaryOutcome::Unsupported(UnsupportedBinary::UsbBridge)
        }
        _ => BinaryOutcome::Unsupported(UnsupportedBinary::Video),
    }
}

/// Reads a `clipboard_data` offer.
///
/// Returns `None` for any other control message, which is what lets the input
/// session see everything that is not a clipboard announcement.
fn read_clipboard_offer(text: &str) -> Option<arcen_protocol::messages::ClipboardDataMsg> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str)
        != Some(arcen_protocol::messages::CLIPBOARD_DATA)
    {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// Frame counts are far below the range where `f64` loses integer precision.
#[allow(clippy::cast_precision_loss, clippy::expect_used)]
#[cfg(test)]
mod tests {
    #[test]
    fn opus_audio_is_labelled_opus_and_decodes_with_the_shared_decoder() {
        let tier = arcen_media::audio::AudioBitrateTier::Kbps128;
        let mut encoder = AudioPacketEncoder::new(AudioEncoding::Opus(tier));
        assert!(
            encoder.opus.is_some(),
            "the shared Opus encoder is available"
        );
        let tone: Vec<i16> = (0..1920)
            .map(|index| {
                let phase = f64::from(index / 2) * 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
                #[allow(clippy::cast_possible_truncation)]
                let sample = (phase.sin() * 8000.0) as i16;
                sample
            })
            .collect();
        let frame = encoder.encode(&tone, 1234).expect("encoded");
        let header = arcen_protocol::wire::decode_audio_header(&frame).expect("header");
        let body = &frame[arcen_protocol::wire::AUDIO_HEADER_SIZE..];
        assert_eq!(header.codec, arcen_protocol::wire::AudioCodec::Opus);
        assert_eq!(header.timestamp_ms, 1234);
        assert!(
            body.len() < 1920 * 2 / 4,
            "Opus is far smaller than PCM: {}",
            body.len()
        );
        let mut decoder = arcen_media::audio::OpusDecoder::new().expect("decoder");
        let mut out = vec![0_i16; 1920];
        decoder.decode(body, &mut out).expect("decodes");
        assert!(out.iter().any(|&sample| sample != 0));

        let mut pcm = AudioPacketEncoder::new(AudioEncoding::Pcm);
        let frame = pcm.encode(&tone, 5).expect("pcm");
        let header = arcen_protocol::wire::decode_audio_header(&frame).expect("header");
        let body = &frame[arcen_protocol::wire::AUDIO_HEADER_SIZE..];
        assert_eq!(header.codec, arcen_protocol::wire::AudioCodec::Pcm);
        assert_eq!(body.len(), 1920 * 2);
    }

    #[tokio::test]
    async fn queued_audio_is_written_before_queued_video() {
        let (bulk_tx, mut bulk) = tokio::sync::mpsc::channel::<Message>(4);
        let (priority_tx, mut priority) = tokio::sync::mpsc::channel::<Message>(4);
        bulk_tx
            .send(Message::Text("video-1".into()))
            .await
            .expect("video");
        bulk_tx
            .send(Message::Text("video-2".into()))
            .await
            .expect("video");
        priority_tx
            .send(Message::Text("audio".into()))
            .await
            .expect("audio");
        drop((bulk_tx, priority_tx));
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = std::sync::Arc::clone(&written);
        let sink = futures_util::sink::unfold((), move |(), message: Message| {
            let record = std::sync::Arc::clone(&record);
            async move {
                record
                    .lock()
                    .expect("record")
                    .push(message.into_text().expect("text").to_string());
                Ok::<(), std::io::Error>(())
            }
        });
        tokio::pin!(sink);
        drive_writer(&mut sink, &mut priority, &mut bulk)
            .await
            .expect("drained");
        assert_eq!(
            *written.lock().expect("written"),
            vec!["audio", "video-1", "video-2"],
            "audio queued last still goes first"
        );
    }

    use super::*;
    use arcen_protocol::wire::{BitDepth, ChromaSubsampling, FrameType, VideoCodec};
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;

    #[derive(Default)]
    struct CollectSink {
        messages: Vec<Message>,
    }

    impl futures_util::Sink<Message> for CollectSink {
        type Error = std::io::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(
            mut self: std::pin::Pin<&mut Self>,
            message: Message,
        ) -> Result<(), Self::Error> {
            self.messages.push(message);
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct HealthRecord {
        capture_wait: Option<f64>,
        encode: Option<f64>,
        send: Option<f64>,
        max_frame: Option<f64>,
    }

    struct HealthVisitor<'a>(&'a mut HealthRecord);

    impl Visit for HealthVisitor<'_> {
        fn record_f64(&mut self, field: &Field, value: f64) {
            match field.name() {
                "mean_capture_wait_ms" => self.0.capture_wait = Some(value),
                "mean_encode_ms" => self.0.encode = Some(value),
                "mean_send_ms" => self.0.send = Some(value),
                "max_frame_ms" => self.0.max_frame = Some(value),
                _ => {}
            }
        }

        fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
    }

    struct HealthLayer {
        records: Arc<Mutex<Vec<HealthRecord>>>,
    }

    impl<S> Layer<S> for HealthLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if event.metadata().target() != arcen_telemetry::names::target::MEDIA {
                return;
            }
            let mut record = HealthRecord::default();
            event.record(&mut HealthVisitor(&mut record));
            if record.encode.is_some() {
                self.records.lock().expect("records lock").push(record);
            }
        }
    }

    fn unit(keyframe: bool) -> EncodedAccessUnit {
        EncodedAccessUnit {
            bytes: vec![0, 0, 0, 1, 0x26],
            keyframe,
            pts: 0,
            timescale: 60,
        }
    }

    #[tokio::test]
    async fn clipboard_sender_emits_only_one_wire_message_per_turn() {
        let bytes = vec![b'z'; arcen_protocol::CHUNK_BYTES + 1];
        let transfer = arcen_protocol::clipboard::ClipboardTransfer::new(
            1,
            arcen_protocol::messages::ClipboardContentKind::TextUtf8,
            bytes,
        )
        .expect("transfer");
        let mut sender = ClipboardSender {
            active: Some(transfer.into_cursor()),
        };
        // Collected from the writer queue rather than a sink: the clipboard
        // sender hands messages to the writer now, like everything else that
        // used to write for itself.
        let (writer, mut queue) = tokio::sync::mpsc::channel::<Message>(8);

        sender.send_one(&writer).await.expect("offer send");
        let offer = queue.try_recv().expect("an offer was queued");
        assert!(offer.is_text());
        assert!(sender.has_work());

        sender.send_one(&writer).await.expect("first chunk send");
        let chunk = queue.try_recv().expect("a chunk was queued");
        assert!(chunk.is_binary());
        assert!(sender.has_work());
    }

    #[test]
    fn capture_stop_is_terminal_not_idle() {
        let error = classify_capture_wait(
            CaptureError::StreamStopped("user stopped sharing".to_owned()),
            true,
            Duration::ZERO,
            Duration::from_secs(10),
        )
        .expect_err("a native stop must end the stream");
        assert!(matches!(
            error,
            StreamError::Capture(CaptureError::StreamStopped(_))
        ));
    }

    #[test]
    fn first_frame_wait_is_bounded_but_later_idle_is_allowed() {
        let limit = Duration::from_millis(50);
        let first = classify_capture_wait(CaptureError::FrameTimeout, false, limit, limit)
            .expect_err("a capture that never proves an image must fail");
        assert!(matches!(
            first,
            StreamError::Capture(CaptureError::FirstFrameTimeout)
        ));

        assert_eq!(
            classify_capture_wait(
                CaptureError::FrameTimeout,
                true,
                Duration::from_secs(60),
                limit,
            )
            .expect("a proven still desktop stays connected"),
            CaptureWait::IdleAfterFirstFrame,
        );
    }

    #[tokio::test]
    async fn post_handshake_writes_have_a_progress_deadline() {
        struct PendingSink;
        impl futures_util::Sink<Message> for PendingSink {
            type Error = std::io::Error;
            fn poll_ready(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Pending
            }
            fn start_send(self: std::pin::Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
                unreachable!("poll_ready never permits a send")
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_close(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let mut sink = PendingSink;
        let started = Instant::now();
        let error = send_message_with_timeout(
            &mut sink,
            Message::Text("health_pong".to_owned()),
            Duration::from_millis(5),
        )
        .await
        .expect_err("a peer that stops reading must not hold the session");
        assert!(matches!(error, StreamError::WriteTimeout));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wedged_producer_teardown_returns_under_its_deadline() {
        let cancelled = ProducerCancel::new();
        let (_tx, mut rx) = tokio::sync::mpsc::channel(1);
        let producer = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(60));
            Ok(())
        });

        let started = Instant::now();
        let error = shutdown_producer_with_timeout(
            &cancelled,
            &mut rx,
            producer,
            Duration::from_millis(20),
        )
        .await
        .expect_err("a wedged native encoder must not hang async teardown");
        assert!(matches!(error, StreamError::Encode(_)));
        assert!(cancelled.is_cancelled());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn dropping_a_producer_cancel_token_requests_shutdown() {
        let token = ProducerCancel::new();
        let shared = token.token();
        assert!(!token.is_cancelled());
        drop(token);
        assert!(shared.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn health_diagnostics_emit_live_stage_latency() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(HealthLayer {
            records: Arc::clone(&records),
        });
        let mut stats = StreamStats {
            frames_captured: 2,
            frames_encoded: 2,
            frames_sent: 2,
            bytes_sent: 128,
            ..Default::default()
        };

        assert!(stats.mean_encode_ms.abs() < f64::EPSILON);
        finalize_timings(
            &mut stats,
            Duration::from_secs(1),
            Timings {
                total_queue: Duration::from_millis(30),
                total_frame: Duration::from_millis(60),
                worst_frame: Duration::from_millis(35),
                total_send: Duration::from_millis(10),
                total_capture_wait: Duration::from_millis(20),
                total_encode: Duration::from_millis(40),
            },
        );

        tracing::subscriber::with_default(subscriber, || emit_health_diagnostics(2, &stats));
        let records = records.lock().expect("records lock");
        let record = records.first().expect("health diagnostic record");
        assert_eq!(record.capture_wait, Some(10.0));
        assert_eq!(record.encode, Some(20.0));
        assert_eq!(record.send, Some(5.0));
        assert_eq!(record.max_frame, Some(35.0));
    }

    #[test]
    fn a_session_missing_its_contract_is_not_reported_as_healthy() {
        // Any nonzero rate used to read "ok", so a session serving four frames
        // a second against sixty looked exactly like one serving sixty.
        assert_eq!(health_verdict(60, 60), "ok");
        assert_eq!(health_verdict(45, 60), "ok");
        assert_eq!(health_verdict(4, 60), "degraded");
        assert_eq!(health_verdict(0, 60), "critical");
        // A still desktop legitimately produces few frames; the threshold is
        // generous so the signal stays useful on idle sessions.
        assert_eq!(health_verdict(30, 60), "ok");
        // No contract to miss.
        assert_eq!(health_verdict(0, 0), "ok");
    }

    #[tokio::test]
    async fn audio_sends_are_counted_as_they_succeed() {
        // A client that disappears mid-drain used to discard the whole batch,
        // so a session that had delivered audio recorded none of it.
        struct FailsAfter(usize, usize);
        impl futures_util::Sink<Message> for FailsAfter {
            type Error = std::io::Error;
            fn poll_ready(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn start_send(
                mut self: std::pin::Pin<&mut Self>,
                _: Message,
            ) -> Result<(), Self::Error> {
                if self.0 >= self.1 {
                    return Err(std::io::Error::other("peer gone"));
                }
                self.0 += 1;
                Ok(())
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_close(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let mut sink = FailsAfter(0, 3);
        let mut clock = AudioTimeline::default();
        let mut sent = 0u64;
        let packets: Vec<Vec<i16>> = (0..5).map(|_| vec![0i16; 960]).collect();
        // Drive the same loop `send_audio` runs, against a sink that fails
        // partway, and assert the successes were committed.
        let mut result = Ok(());
        for packet in packets {
            let payload = encode_audio_packet(&packet, clock.next());
            if let Err(error) =
                futures_util::SinkExt::send(&mut sink, Message::Binary(payload)).await
            {
                result = Err(StreamError::PeerGone(error.to_string()));
                break;
            }
            sent += 1;
        }
        assert!(result.is_err(), "the sink was supposed to fail");
        assert_eq!(sent, 3, "the three delivered packets must still be counted");
    }

    #[test]
    fn a_health_ping_is_answered_rather_than_parsed_as_input() {
        // Control messages fell through to the input parser, where they were
        // counted as unsupported and dropped. A Deck showing host health
        // showed nothing, and the count that would have revealed it is not in
        // the stream summary either.
        let mut stats = StreamStats::default();
        let request = serde_json::json!({
            "type": arcen_protocol::messages::HEALTH_PING,
            "timestamp_ms": 1_234_u64,
            "sequence": 7_u64,
        })
        .to_string();
        let Some(Incoming::Reply(reply)) = handle_control(&request, &mut stats, None, None) else {
            panic!("a health_ping must be answered");
        };
        let response: arcen_protocol::messages::HealthPongMsg =
            serde_json::from_str(&reply).expect("the Deck must be able to parse this");
        assert_eq!(response.msg_type, arcen_protocol::messages::HEALTH_PONG);
        assert_eq!(response.ping_timestamp_ms, 1_234);
        assert_eq!(response.sequence, 7);
        assert_eq!(response.server_state, "streaming");
    }

    #[test]
    fn a_full_frame_request_is_counted_and_published_to_the_encoder() {
        let mut stats = StreamStats::default();
        let requests = std::sync::atomic::AtomicU64::new(0);
        let mut last_request = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);
        let request =
            serde_json::json!({ "type": arcen_protocol::messages::REQUEST_FULL_FRAME }).to_string();
        assert!(matches!(
            handle_control(
                &request,
                &mut stats,
                Some(&requests),
                Some(&mut last_request)
            ),
            Some(Incoming::Continue)
        ));
        assert_eq!(stats.full_frame_requests, 1);
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the blocking encoder thread must see the request"
        );
        assert!(matches!(
            handle_control(
                &request,
                &mut stats,
                Some(&requests),
                Some(&mut last_request)
            ),
            Some(Incoming::Continue)
        ));
        assert_eq!(stats.full_frame_requests, 2);
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "bursts should coalesce the same way Linux's IDR guard does"
        );
    }

    #[test]
    fn input_is_still_input() {
        // The dispatch must not swallow messages it does not recognise, or
        // every pointer move becomes an unhandled control message.
        let mut stats = StreamStats::default();
        let move_event = serde_json::json!({
            "type": "mouse_move",
            "x": 0.5, "y": 0.5,
            "sequence": 1, "timestamp_ns": 0,
        })
        .to_string();
        assert!(handle_control(&move_event, &mut stats, None, None).is_none());
    }

    #[test]
    fn audio_packets_advance_by_their_own_duration_not_by_the_wall_clock() {
        // Anchored to the wire clock once, then advanced by exactly one packet
        // duration — the Linux Pier does the same. Stamping each packet with
        // the wall clock carries drain jitter into the presentation times, and
        // a player spacing audio by those timestamps reproduces it as audible
        // unevenness. The packets are 20 ms of sound whenever they were taken.
        let mut clock = AudioTimeline::default();
        let first = clock.next();
        let second = clock.next();
        let third = clock.next();
        let step = u32::from(arcen_media::audio::AUDIO_V1_FRAME_DURATION_MS);
        assert_eq!(second.wrapping_sub(first), step);
        assert_eq!(third.wrapping_sub(second), step);

        // The anchor is the wire clock, so audio and video share an origin.
        let now = arcen_protocol::wire::now_wire_timestamp_ms();
        assert!(
            now.wrapping_sub(first) < 5_000,
            "audio must start on the same clock the video headers use",
        );
    }

    #[test]
    fn a_stream_that_lost_its_client_still_reports_what_it_sent() {
        // Observed on the lab Mac: a session streamed for three and a half
        // minutes, the client was killed, and SESSION_END recorded
        // frames_sent = 0 with no latency summary at all. The count had been
        // hardcoded to zero on every error path because the error carried no
        // statistics, so the ordinary way a remote session ends looked
        // identical to one that never produced a picture.
        let stats = StreamStats {
            frames_sent: 6_142,
            bytes_sent: 90_210_048,
            ..Default::default()
        };
        let ended = StreamEnded {
            stats,
            error: StreamError::PeerGone("connection lost".to_owned()),
        };
        assert_eq!(ended.stats.frames_sent, 6_142);
        assert_eq!(ended.to_string(), "client gone: connection lost");
    }

    #[test]
    fn the_header_describes_the_codec_the_decoder_will_receive() {
        let hevc = header_for(
            &unit(false),
            EncoderCodec::Hevc,
            CapturePixelFormat::Nv12VideoRange,
            0,
        );
        assert_eq!(hevc[0], FrameType::VideoH265 as u8);
        assert_eq!(hevc[1], VideoCodec::H265 as u8);

        let h264 = header_for(
            &unit(false),
            EncoderCodec::H264,
            CapturePixelFormat::Nv12VideoRange,
            0,
        );
        assert_eq!(h264[0], FrameType::VideoH264 as u8);
        assert_eq!(h264[1], VideoCodec::H264 as u8);
    }

    #[test]
    fn keyframes_are_marked_so_a_client_knows_where_it_can_start() {
        let key = header_for(
            &unit(true),
            EncoderCodec::Hevc,
            CapturePixelFormat::Nv12VideoRange,
            0,
        );
        assert_eq!(
            key[3] & arcen_protocol::wire::VIDEO_KEYFRAME_FLAG,
            arcen_protocol::wire::VIDEO_KEYFRAME_FLAG
        );

        let inter = header_for(
            &unit(false),
            EncoderCodec::Hevc,
            CapturePixelFormat::Nv12VideoRange,
            0,
        );
        assert_eq!(inter[3] & arcen_protocol::wire::VIDEO_KEYFRAME_FLAG, 0);
    }

    #[test]
    fn chroma_and_depth_follow_the_captured_format() {
        // Getting this wrong hands a decoder the wrong picture shape, which is
        // worse than refusing the mode.
        let eight_bit = header_for(
            &unit(false),
            EncoderCodec::Hevc,
            CapturePixelFormat::Nv12VideoRange,
            0,
        );
        assert_eq!(eight_bit[2], ChromaSubsampling::Yuv420 as u8);

        let grading = header_for(
            &unit(false),
            EncoderCodec::Hevc,
            CapturePixelFormat::FourFourFourTenBit,
            0,
        );
        assert_eq!(grading[2], ChromaSubsampling::Yuv444 as u8);
        let decoded = arcen_protocol::wire::decode_video_header(&grading).expect("parses");
        assert_eq!(decoded.bit_depth(), Ok(BitDepth::Ten));
    }

    #[test]
    fn grading_headers_say_full_range_on_every_monitor() {
        // The Grading capture is xf44 and its SPS says full range; a header
        // saying limited would have the Deck expand codes that were never
        // compressed. Region headers are what every monitor of a multi-display
        // session carries, so they are checked as well as the legacy header.
        let capture = crate::capture::CaptureConfig::grading(1, 1920, 1080, 30);
        let legacy = header_for(&unit(true), EncoderCodec::Hevc, capture.pixel_format, 0);
        let region = region_header_for(
            &unit(true),
            EncoderCodec::Hevc,
            capture.pixel_format,
            0,
            arcen_media::SessionMonitorId::new(3).expect("monitor id"),
            arcen_media::TopologyGeneration::new(1).expect("generation"),
            arcen_media::MediaStreamEpoch::new(1).expect("epoch"),
        );
        for bytes in [legacy, region] {
            let decoded = arcen_protocol::wire::decode_video_header(&bytes).expect("parses");
            assert_eq!(decoded.chroma, ChromaSubsampling::Yuv444);
            assert_eq!(decoded.bit_depth(), Ok(BitDepth::Ten));
            assert_eq!(
                decoded.color_range(),
                arcen_protocol::wire::ColorRange::Full
            );
        }
        let fast = header_for(
            &unit(true),
            EncoderCodec::Hevc,
            CapturePixelFormat::Nv12VideoRange,
            0,
        );
        let decoded = arcen_protocol::wire::decode_video_header(&fast).expect("parses");
        assert_eq!(
            decoded.color_range(),
            arcen_protocol::wire::ColorRange::Limited
        );
    }

    #[test]
    fn a_header_round_trips_through_the_shared_decoder() {
        // The Deck decodes this, so a header it cannot read is a black window.
        let bytes = header_for(
            &unit(true),
            EncoderCodec::Hevc,
            CapturePixelFormat::FourFourFourTenBit,
            1234,
        );
        let decoded =
            arcen_protocol::wire::decode_video_header(&bytes).expect("the Deck must parse this");
        assert_eq!(decoded.frame_type, FrameType::VideoH265);
        assert_eq!(decoded.codec, VideoCodec::H265);
        assert_eq!(decoded.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(decoded.timestamp_ms, 1234);
        assert_eq!(decoded.monitor_id, 0);
    }

    #[test]
    fn region_headers_carry_nonzero_monitor_and_generation() {
        let bytes = region_header_for(
            &unit(true),
            EncoderCodec::Hevc,
            CapturePixelFormat::Nv12VideoRange,
            77,
            arcen_media::SessionMonitorId::new(2).expect("monitor"),
            arcen_media::TopologyGeneration::new(3).expect("generation"),
            arcen_media::MediaStreamEpoch::new(3).expect("epoch"),
        );
        let decoded =
            arcen_protocol::wire::decode_video_header(&bytes).expect("region header parses");
        assert_eq!(decoded.frame_type, FrameType::RegionVideoH265);
        assert_eq!(decoded.monitor_id, 2);
        assert_eq!(decoded.topology_generation, 3);
        assert_eq!(decoded.stream_epoch, 3);
    }

    #[test]
    fn the_header_is_the_legacy_single_monitor_size() {
        let bytes = header_for(
            &unit(false),
            EncoderCodec::Hevc,
            CapturePixelFormat::Nv12VideoRange,
            0,
        );
        assert_eq!(bytes.len(), arcen_protocol::wire::VIDEO_HEADER_SIZE);
    }
}

#[cfg(test)]
mod damage_policy_tests {
    use super::{KEEPALIVE, observe_damage};
    use crate::capture::{DamageRect, FrameDamage};
    use arcen_keel::{EmitMode, ExternalDamage, IdleCadence};
    use std::time::Duration;

    const WIDTH: usize = 1920;
    const HEIGHT: usize = 1080;

    fn fixture() -> (ExternalDamage, IdleCadence) {
        (
            ExternalDamage::new(WIDTH, HEIGHT).expect("grid"),
            IdleCadence::new(KEEPALIVE),
        )
    }

    fn settled(damage: &mut ExternalDamage, cadence: &mut IdleCadence) {
        // Get past the first frame, which is always owed.
        observe_damage(
            &FrameDamage::Rects(vec![DamageRect {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            }]),
            damage,
            cadence,
            WIDTH,
            HEIGHT,
        );
        assert_eq!(
            cadence.decision(false, Duration::ZERO),
            Some(EmitMode::FirstFrame)
        );
        cadence.on_submitted();
        damage.reset();
    }

    #[test]
    fn a_still_desktop_is_not_worth_encoding() {
        let (mut damage, mut cadence) = fixture();
        settled(&mut damage, &mut cadence);
        observe_damage(
            &FrameDamage::Rects(Vec::new()),
            &mut damage,
            &mut cadence,
            WIDTH,
            HEIGHT,
        );
        assert_eq!(cadence.decision(false, Duration::ZERO), None);
        assert_eq!(damage.summary().dirty_blocks, 0);
    }

    #[test]
    fn a_still_desktop_still_owes_a_keepalive() {
        let (mut damage, mut cadence) = fixture();
        settled(&mut damage, &mut cadence);
        observe_damage(
            &FrameDamage::Rects(Vec::new()),
            &mut damage,
            &mut cadence,
            WIDTH,
            HEIGHT,
        );
        assert_eq!(
            cadence.decision(false, KEEPALIVE),
            Some(EmitMode::Keepalive)
        );
    }

    #[test]
    fn unknown_damage_marks_the_whole_frame_and_counts_as_activity() {
        // Being told nothing must never read as "nothing changed".
        let (mut damage, mut cadence) = fixture();
        settled(&mut damage, &mut cadence);
        observe_damage(
            &FrameDamage::Unknown,
            &mut damage,
            &mut cadence,
            WIDTH,
            HEIGHT,
        );
        let summary = damage.summary();
        assert_eq!(summary.dirty_blocks, summary.total_blocks);
        assert_eq!(
            cadence.decision(false, Duration::ZERO),
            Some(EmitMode::Activity)
        );
    }

    #[test]
    fn a_changed_region_is_activity_covering_only_its_blocks() {
        let (mut damage, mut cadence) = fixture();
        settled(&mut damage, &mut cadence);
        observe_damage(
            &FrameDamage::Rects(vec![DamageRect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            }]),
            &mut damage,
            &mut cadence,
            WIDTH,
            HEIGHT,
        );
        let summary = damage.summary();
        assert_eq!(summary.dirty_blocks, 4, "a 32x32 rect is four 16x16 blocks");
        assert!(summary.dirty_blocks < summary.total_blocks);
        assert_eq!(
            cadence.decision(false, Duration::ZERO),
            Some(EmitMode::Activity)
        );
    }

    #[test]
    fn damage_accumulates_across_frames_that_were_not_sent() {
        // A superseded frame's damage must survive it. Losing it would let the
        // next frame claim a region was clean when it never was.
        let (mut damage, mut cadence) = fixture();
        settled(&mut damage, &mut cadence);
        for x in [0_u32, 64, 128] {
            observe_damage(
                &FrameDamage::Rects(vec![DamageRect {
                    x,
                    y: 0,
                    width: 16,
                    height: 16,
                }]),
                &mut damage,
                &mut cadence,
                WIDTH,
                HEIGHT,
            );
        }
        assert_eq!(damage.summary().dirty_blocks, 3);
    }

    #[test]
    fn a_recovery_request_is_served_from_a_still_desktop() {
        // A Deck that cannot decode asks for a full frame, and a still desktop
        // is exactly when nothing else would produce one.
        let (mut damage, mut cadence) = fixture();
        settled(&mut damage, &mut cadence);
        observe_damage(
            &FrameDamage::Rects(Vec::new()),
            &mut damage,
            &mut cadence,
            WIDTH,
            HEIGHT,
        );
        assert_eq!(cadence.decision(true, Duration::ZERO), Some(EmitMode::Idr));
    }

    #[test]
    fn a_rectangle_beyond_the_surface_does_not_escape_the_grid() {
        let (mut damage, mut cadence) = fixture();
        settled(&mut damage, &mut cadence);
        observe_damage(
            &FrameDamage::Rects(vec![DamageRect {
                x: u32::MAX,
                y: u32::MAX,
                width: u32::MAX,
                height: u32::MAX,
            }]),
            &mut damage,
            &mut cadence,
            WIDTH,
            HEIGHT,
        );
        let summary = damage.summary();
        assert!(summary.dirty_blocks <= summary.total_blocks);
    }
}
