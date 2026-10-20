//! The Pier's side of a remote-monitor session's output socket: the current
//! outputs, and output transactions (resize, topology change).
//!
//! The socket is proven the session's own before anything is sent: it lies
//! directly in the session's private runtime directory, and its peer is the
//! session agent (user and process id from the ready message).

use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::{OutputCommand, OutputPlacement, OutputReply, WaylandAgentReady};

/// How long a state query may take.
const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a reconfiguration may take: each output negotiated (5 s) and
/// placed (5 s), for up to four outputs, with margin.
pub const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(50);
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// An output socket failure.
#[derive(Debug, Error)]
pub enum OutputClientError {
    #[error("the session has no output socket (its outputs are fixed)")]
    Unavailable,
    #[error("output socket I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("output socket peer is not the session agent")]
    ForeignPeer,
    #[error("the session did not answer in time")]
    Timeout,
    #[error("the session's answer is malformed")]
    Malformed,
    /// The session refused; `live` is what it kept.
    #[error("the session refused: {reason}")]
    Refused {
        /// The session's reason.
        reason: String,
        /// The outputs that are live after the refusal.
        live: Box<OutputReply>,
    },
}

/// Sends one command and reads its reply.
async fn exchange(
    ready: &WaylandAgentReady,
    uid: u32,
    agent_pid: u32,
    command: &OutputCommand,
    timeout: Duration,
) -> Result<OutputReply, OutputClientError> {
    let socket = ready
        .output_socket
        .as_deref()
        .ok_or(OutputClientError::Unavailable)?;
    if socket.parent() != Some(ready.runtime_dir.as_path()) {
        return Err(OutputClientError::Unavailable);
    }
    tokio::time::timeout(timeout, async {
        let stream = UnixStream::connect(socket).await?;
        verify_peer(&stream, uid, agent_pid)?;
        let (reader, mut writer) = stream.into_split();
        let mut line = serde_json::to_vec(command).map_err(|_| OutputClientError::Malformed)?;
        line.push(b'\n');
        writer.write_all(&line).await?;
        let mut reader = BufReader::new(reader).take(MAX_REPLY_BYTES as u64);
        let mut reply = String::new();
        reader.read_line(&mut reply).await?;
        let reply: OutputReply =
            serde_json::from_str(&reply).map_err(|_| OutputClientError::Malformed)?;
        if !reply.is_consistent() {
            return Err(OutputClientError::Malformed);
        }
        Ok(reply)
    })
    .await
    .map_err(|_| OutputClientError::Timeout)?
}

fn verify_peer(stream: &UnixStream, uid: u32, agent_pid: u32) -> Result<(), OutputClientError> {
    let credentials = stream.peer_cred()?;
    let pid = credentials.pid().and_then(|pid| u32::try_from(pid).ok());
    if credentials.uid() == uid && pid == Some(agent_pid) {
        Ok(())
    } else {
        Err(OutputClientError::ForeignPeer)
    }
}

/// The session's current outputs and generation.
///
/// # Errors
///
/// [`OutputClientError`].
pub async fn current(
    ready: &WaylandAgentReady,
    uid: u32,
    agent_pid: u32,
) -> Result<OutputReply, OutputClientError> {
    // The agent answers `hello` from a snapshot; a miss is a transient
    // connection problem or a session that is gone, so ask three times.
    let mut attempt = 0;
    loop {
        attempt += 1;
        match exchange(ready, uid, agent_pid, &OutputCommand::Hello, HELLO_TIMEOUT).await {
            Ok(reply) => return Ok(reply),
            Err(error) if attempt >= 3 => return Err(error),
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

/// Moves the session to `outputs` as output generation `generation`. On
/// refusal the session kept its previous outputs, which the error's reply
/// reports through [`current`].
///
/// # Errors
///
/// [`OutputClientError::Refused`] with the session's reason, or a transport
/// failure.
pub async fn configure(
    ready: &WaylandAgentReady,
    uid: u32,
    agent_pid: u32,
    generation: u64,
    outputs: Vec<OutputPlacement>,
) -> Result<OutputReply, OutputClientError> {
    let reply = exchange(
        ready,
        uid,
        agent_pid,
        &OutputCommand::Configure {
            generation,
            outputs,
        },
        CONFIGURE_TIMEOUT,
    )
    .await?;
    if reply.ok && reply.generation == generation {
        Ok(reply)
    } else {
        Err(OutputClientError::Refused {
            reason: reply
                .error
                .clone()
                .unwrap_or_else(|| "the generation did not advance".to_owned()),
            live: Box::new(reply),
        })
    }
}

/// How long recording the outputs again for wide capture may take: each
/// stream announces its node within 10 s.
const WIDE_STREAMS_TIMEOUT: Duration = Duration::from_secs(15);

/// The session's wide-capture streams of output generation `generation`
/// (`RecordMonitor` of each output, for a Grading or HDR capture epoch),
/// recorded on first use.
///
/// # Errors
///
/// [`OutputClientError::Refused`] when the session's compositor does not
/// compose in a wide colour state or the generation moved on, or a
/// transport failure.
pub async fn wide_streams(
    ready: &WaylandAgentReady,
    uid: u32,
    agent_pid: u32,
    generation: u64,
) -> Result<OutputReply, OutputClientError> {
    let reply = exchange(
        ready,
        uid,
        agent_pid,
        &OutputCommand::WideStreams { generation },
        WIDE_STREAMS_TIMEOUT,
    )
    .await?;
    if reply.ok && reply.generation == generation {
        Ok(reply)
    } else {
        Err(OutputClientError::Refused {
            reason: reply
                .error
                .clone()
                .unwrap_or_else(|| "the generation moved on".to_owned()),
            live: Box::new(reply),
        })
    }
}
