//! A second, higher-priority QUIC stream for media that must not wait.
//!
//! The session stream is reliable and ordered, so a frame written to it waits
//! behind every byte written before it — including every video byte the
//! connection has accepted but not yet sent. Audio written after a large
//! video frame therefore arrives late by however long that frame takes, and
//! measured on a 5 Mbit/s path that was up to 450 ms.
//!
//! This stream is unidirectional, host to client, opened with a higher
//! [`quinn::SendStream::set_priority`] than the session stream, so the
//! connection transmits its bytes first whenever both have data waiting. It
//! starts with [`arcen_protocol::AUDIO_PRIORITY_STREAM_V1`] and then carries
//! binary media frames, each prefixed with its length as a big-endian `u32`.

use quinn::{Connection, RecvStream, SendStream};

use arcen_protocol::{AUDIO_PRIORITY_MAX_FRAME_BYTES, AUDIO_PRIORITY_STREAM_V1};

/// The priority the audio stream is given. The session stream is left at
/// quinn's default of zero.
pub const AUDIO_PRIORITY_STREAM_PRIORITY: i32 = 10;

/// Why the priority stream could not be used.
#[derive(Debug)]
pub enum PriorityStreamError {
    /// The connection or stream failed.
    Connection(String),
    /// The peer's stream did not begin with the expected preface.
    Preface,
    /// A frame is larger than [`AUDIO_PRIORITY_MAX_FRAME_BYTES`].
    Oversize(usize),
}

impl std::fmt::Display for PriorityStreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(detail) => write!(formatter, "priority stream: {detail}"),
            Self::Preface => formatter.write_str("priority stream: unexpected preface"),
            Self::Oversize(length) => write!(
                formatter,
                "priority stream: {length}-byte frame exceeds {AUDIO_PRIORITY_MAX_FRAME_BYTES}"
            ),
        }
    }
}

impl std::error::Error for PriorityStreamError {}

/// Opens the audio priority stream towards the client.
///
/// # Errors
///
/// Returns [`PriorityStreamError::Connection`] when the stream cannot be
/// opened or the preface cannot be written.
pub async fn open_audio_priority_stream(
    connection: &Connection,
) -> Result<SendStream, PriorityStreamError> {
    let mut send = connection
        .open_uni()
        .await
        .map_err(|error| PriorityStreamError::Connection(error.to_string()))?;
    send.set_priority(AUDIO_PRIORITY_STREAM_PRIORITY)
        .map_err(|error| PriorityStreamError::Connection(error.to_string()))?;
    send.write_all(AUDIO_PRIORITY_STREAM_V1)
        .await
        .map_err(|error| PriorityStreamError::Connection(error.to_string()))?;
    Ok(send)
}

/// Writes one frame to the priority stream.
///
/// # Errors
///
/// Returns [`PriorityStreamError::Oversize`] for a frame beyond the bound and
/// [`PriorityStreamError::Connection`] when the stream fails.
pub async fn write_priority_frame(
    send: &mut SendStream,
    frame: &[u8],
) -> Result<(), PriorityStreamError> {
    if frame.len() > AUDIO_PRIORITY_MAX_FRAME_BYTES {
        return Err(PriorityStreamError::Oversize(frame.len()));
    }
    let mut buffer = Vec::with_capacity(4 + frame.len());
    buffer.extend_from_slice(&u32::try_from(frame.len()).unwrap_or(u32::MAX).to_be_bytes());
    buffer.extend_from_slice(frame);
    send.write_all(&buffer)
        .await
        .map_err(|error| PriorityStreamError::Connection(error.to_string()))
}

/// Accepts the audio priority stream from the host.
///
/// # Errors
///
/// Returns [`PriorityStreamError::Connection`] when the connection ends first
/// and [`PriorityStreamError::Preface`] for a stream that is not this one.
pub async fn accept_audio_priority_stream(
    connection: &Connection,
) -> Result<RecvStream, PriorityStreamError> {
    let mut recv = connection
        .accept_uni()
        .await
        .map_err(|error| PriorityStreamError::Connection(error.to_string()))?;
    let mut preface = [0_u8; AUDIO_PRIORITY_STREAM_V1.len()];
    recv.read_exact(&mut preface)
        .await
        .map_err(|error| PriorityStreamError::Connection(error.to_string()))?;
    if &preface != AUDIO_PRIORITY_STREAM_V1 {
        return Err(PriorityStreamError::Preface);
    }
    Ok(recv)
}

/// Reads the next frame, or `None` when the host finished the stream.
///
/// # Errors
///
/// Returns [`PriorityStreamError::Oversize`] for a declared length beyond the
/// bound, which a client treats as a broken host, and
/// [`PriorityStreamError::Connection`] when the stream fails mid-frame.
pub async fn read_priority_frame(
    recv: &mut RecvStream,
) -> Result<Option<Vec<u8>>, PriorityStreamError> {
    let mut length = [0_u8; 4];
    match recv.read_exact(&mut length).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(error) => return Err(PriorityStreamError::Connection(error.to_string())),
    }
    let length = usize::try_from(u32::from_be_bytes(length)).unwrap_or(usize::MAX);
    if length > AUDIO_PRIORITY_MAX_FRAME_BYTES {
        return Err(PriorityStreamError::Oversize(length));
    }
    let mut frame = vec![0_u8; length];
    recv.read_exact(&mut frame)
        .await
        .map_err(|error| PriorityStreamError::Connection(error.to_string()))?;
    Ok(Some(frame))
}

/// What one [`PriorityAudio::send`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriorityAudioSend {
    /// The stream was opened by this send, and the frame sent on it.
    Opened,
    /// The frame was sent on the already-open stream.
    Sent,
    /// The stream failed earlier; the caller sends on the session stream.
    Unusable,
    /// The stream could not be opened; the caller sends on the session
    /// stream from now on.
    OpenFailed(String),
    /// The stream failed while writing; the caller sends on the session
    /// stream from now on.
    WriteFailed(String),
}

impl PriorityAudioSend {
    /// Whether the frame reached the priority stream.
    #[must_use]
    pub const fn delivered(&self) -> bool {
        matches!(self, Self::Opened | Self::Sent)
    }
}

/// A host's side of the Deck's audio priority stream: opened on the first
/// audio frame after the Deck opts in, and abandoned for the session stream
/// the first time it fails, so audio is never lost to the attempt.
///
/// A Pier that relays a session message by message diverts audio frames here
/// instead of queueing them behind video the session stream has already
/// accepted. The frames are the same bytes the session stream would carry.
pub struct PriorityAudio {
    connection: Connection,
    stream: Option<SendStream>,
    failed: bool,
}

impl std::fmt::Debug for PriorityAudio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PriorityAudio")
            .field("open", &self.stream.is_some())
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl PriorityAudio {
    /// A priority stream on `connection`, not yet opened.
    #[must_use]
    pub const fn new(connection: Connection) -> Self {
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

    /// Sends one audio frame, opening the stream on first use.
    pub async fn send(&mut self, frame: &[u8]) -> PriorityAudioSend {
        if self.failed {
            return PriorityAudioSend::Unusable;
        }
        let opened = if self.stream.is_none() {
            match open_audio_priority_stream(&self.connection).await {
                Ok(stream) => {
                    self.stream = Some(stream);
                    true
                }
                Err(error) => {
                    self.failed = true;
                    return PriorityAudioSend::OpenFailed(error.to_string());
                }
            }
        } else {
            false
        };
        let Some(stream) = self.stream.as_mut() else {
            return PriorityAudioSend::Unusable;
        };
        match write_priority_frame(stream, frame).await {
            Ok(()) if opened => PriorityAudioSend::Opened,
            Ok(()) => PriorityAudioSend::Sent,
            Err(error) => {
                self.stream = None;
                self.failed = true;
                PriorityAudioSend::WriteFailed(error.to_string())
            }
        }
    }

    /// Finishes the stream, if it was opened.
    pub fn finish(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.finish();
        }
    }
}
