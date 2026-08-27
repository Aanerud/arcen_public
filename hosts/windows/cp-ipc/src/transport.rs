//! Transport-agnostic framing for the control pipe.
//!
//! The wire protocol ([`crate::CpMessage`]) is framed identically no matter what
//! carries it: on Windows it rides a SYSTEM-only named pipe; in tests and the
//! pure integration harness it rides an in-memory byte duplex. Both use
//! [`StreamFrames`], which performs **exact framed reads** — a 4-byte length
//! prefix (bounded before allocation) followed by exactly that many body bytes —
//! so the higher layers never see a partial or oversized message.
//!
//! Nothing here authenticates a peer. The platform transport wraps its verified
//! handle in a [`FrameIo`]; this module only moves bytes.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};

use crate::{CpMessage, ProtocolError, MAX_FRAME_LEN};

/// Errors from moving a framed message across a transport.
#[derive(Debug)]
pub enum TransportError {
    /// The frame or message failed protocol validation.
    Protocol(ProtocolError),
    /// The underlying byte transport failed.
    Io(String),
    /// The peer closed the transport cleanly (EOF mid-frame or at a boundary).
    Closed,
    /// A message arrived that was not expected in the current protocol phase.
    Unexpected(&'static str),
    /// A transport-level replay nonce was reused.
    Replay(u64),
    /// The platform peer check refused the connection (wrong process/identity).
    PeerRejected,
    /// A sealed credential could not be opened or a session rule was violated.
    Session(crate::cp_session::SessionError),
    /// The platform provider could not arm or notify its native credential state.
    ArmFailed(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(error) => write!(f, "protocol error: {error}"),
            Self::Io(detail) => write!(f, "transport io error: {detail}"),
            Self::Closed => f.write_str("transport closed by peer"),
            Self::Unexpected(what) => write!(f, "unexpected message: {what}"),
            Self::Replay(nonce) => write!(f, "replayed transport nonce: {nonce}"),
            Self::PeerRejected => f.write_str("peer identity check failed"),
            Self::Session(error) => write!(f, "credential session error: {error}"),
            Self::ArmFailed(detail) => write!(f, "credential arm failed: {detail}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<ProtocolError> for TransportError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// A bidirectional, message-framed transport.
pub trait FrameIo {
    /// Read, bound-check, decode, and validate exactly one message.
    fn read_message(&mut self) -> Result<CpMessage, TransportError>;
    /// Encode and write exactly one message, then flush.
    fn write_message(&mut self, message: &CpMessage) -> Result<(), TransportError>;
}

/// Length-prefixed framing over any blocking byte stream (a named pipe handle,
/// an in-memory duplex, a socket).
pub struct StreamFrames<S> {
    stream: S,
}

impl<S: Read + Write> StreamFrames<S> {
    pub fn new(stream: S) -> Self {
        Self { stream }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S: Read + Write> FrameIo for StreamFrames<S> {
    fn read_message(&mut self) -> Result<CpMessage, TransportError> {
        let mut prefix = [0u8; 4];
        read_exact(&mut self.stream, &mut prefix)?;
        let declared = u32::from_be_bytes(prefix) as usize;
        if declared > MAX_FRAME_LEN {
            return Err(TransportError::Protocol(ProtocolError::FrameTooLarge {
                declared,
                max: MAX_FRAME_LEN,
            }));
        }
        let mut body = vec![0u8; declared];
        read_exact(&mut self.stream, &mut body)?;
        CpMessage::decode_body(&body).map_err(TransportError::Protocol)
    }

    fn write_message(&mut self, message: &CpMessage) -> Result<(), TransportError> {
        let framed = message.encode().map_err(TransportError::Protocol)?;
        self.stream
            .write_all(&framed)
            .map_err(|error| TransportError::Io(error.to_string()))?;
        self.stream
            .flush()
            .map_err(|error| TransportError::Io(error.to_string()))
    }
}

fn read_exact<S: Read>(stream: &mut S, buffer: &mut [u8]) -> Result<(), TransportError> {
    match stream.read_exact(buffer) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(TransportError::Closed)
        }
        Err(error) => Err(TransportError::Io(error.to_string())),
    }
}

// ---------------------------------------------------------------------------
// In-memory duplex — a blocking byte "socketpair" used by tests and the pure
// integration harness. It exercises the exact same StreamFrames path the Windows
// named pipe uses, and lets a test inject raw (even malformed/oversized) bytes.
// ---------------------------------------------------------------------------

struct Channel {
    buffer: Mutex<ChannelState>,
    signal: Condvar,
}

struct ChannelState {
    bytes: VecDeque<u8>,
    closed: bool,
}

/// One end of an in-memory blocking byte duplex.
pub struct MemStream {
    inbound: Arc<Channel>,
    outbound: Arc<Channel>,
}

impl Read for MemStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = self.inbound.buffer.lock().expect("mem channel lock");
        loop {
            if let Some(&_front) = state.bytes.front() {
                let count = out.len().min(state.bytes.len());
                for slot in out.iter_mut().take(count) {
                    *slot = state.bytes.pop_front().expect("byte present");
                }
                return Ok(count);
            }
            if state.closed {
                return Ok(0); // clean EOF
            }
            state = self.inbound.signal.wait(state).expect("mem channel wait");
        }
    }
}

impl Write for MemStream {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut state = self.outbound.buffer.lock().expect("mem channel lock");
        if state.closed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "peer closed",
            ));
        }
        state.bytes.extend(data.iter().copied());
        self.outbound.signal.notify_all();
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for MemStream {
    fn drop(&mut self) {
        // Closing our outbound direction lets the peer's reads see EOF.
        if let Ok(mut state) = self.outbound.buffer.lock() {
            state.closed = true;
        }
        self.outbound.signal.notify_all();
    }
}

/// Build a connected pair of in-memory streams.
pub fn mem_duplex() -> (MemStream, MemStream) {
    let a_to_b = Arc::new(Channel {
        buffer: Mutex::new(ChannelState {
            bytes: VecDeque::new(),
            closed: false,
        }),
        signal: Condvar::new(),
    });
    let b_to_a = Arc::new(Channel {
        buffer: Mutex::new(ChannelState {
            bytes: VecDeque::new(),
            closed: false,
        }),
        signal: Condvar::new(),
    });
    let a = MemStream {
        inbound: Arc::clone(&b_to_a),
        outbound: Arc::clone(&a_to_b),
    };
    let b = MemStream {
        inbound: a_to_b,
        outbound: b_to_a,
    };
    (a, b)
}

/// Build a connected pair already wrapped in [`StreamFrames`].
pub fn mem_frame_duplex() -> (StreamFrames<MemStream>, StreamFrames<MemStream>) {
    let (a, b) = mem_duplex();
    (StreamFrames::new(a), StreamFrames::new(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hello, Role, PROTOCOL_VERSION};

    #[test]
    fn frames_roundtrip_over_the_duplex() {
        let (mut a, mut b) = mem_frame_duplex();
        let hello = CpMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            role: Role::Provider,
            nonce: 1,
        });
        let worker = std::thread::spawn(move || a.read_message());
        b.write_message(&hello).expect("write");
        assert_eq!(worker.join().expect("join").expect("read"), hello);
    }

    #[test]
    fn oversized_prefix_is_refused_without_allocation() {
        let (a, mut b) = mem_duplex();
        let prefix = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes();
        b.write_all(&prefix).expect("write prefix");
        b.flush().expect("flush");
        let mut frames = StreamFrames::new(a);
        assert!(matches!(
            frames.read_message(),
            Err(TransportError::Protocol(
                ProtocolError::FrameTooLarge { .. }
            ))
        ));
    }

    #[test]
    fn eof_before_a_frame_reports_closed() {
        let (a, b) = mem_duplex();
        drop(b);
        let mut frames = StreamFrames::new(a);
        assert!(matches!(frames.read_message(), Err(TransportError::Closed)));
    }
}
