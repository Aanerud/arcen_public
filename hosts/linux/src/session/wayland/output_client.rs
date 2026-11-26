// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
    /// The desktop's streams stayed closed (it is locked) for as long as an
    /// attachment waits.
    #[error("the desktop stayed locked: its streams were not recorded again in time")]
    StreamsUnavailable,
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

/// The session's current outputs once their streams are live. A desktop
/// that was locked records its streams again after the unlock; an
/// attachment waits for that as long as the shared
/// [`attach_wait`](arcen_session::desktop_session::remote_access::attach_wait)
/// allows.
///
/// # Errors
///
/// [`OutputClientError::StreamsUnavailable`] when they stay closed, or what
/// [`current`] returns.
pub async fn current_live(
    ready: &WaylandAgentReady,
    uid: u32,
    agent_pid: u32,
) -> Result<OutputReply, OutputClientError> {
    use arcen_session::desktop_session::remote_access::{attach_wait, AttachWait};

    let started = tokio::time::Instant::now();
    let mut logged = false;
    loop {
        let reply = current(ready, uid, agent_pid).await?;
        match attach_wait(reply.streams_unavailable, started.elapsed()) {
            AttachWait::Proceed => return Ok(reply),
            AttachWait::Refuse => return Err(OutputClientError::StreamsUnavailable),
            AttachWait::Poll(delay) => {
                if !logged {
                    logged = true;
                    tracing::info!(
                        target: crate::logging::target::DISPLAY,
                        generation = reply.generation,
                        "the desktop's streams are closed (locked); waiting for them to be recorded again"
                    );
                }
                tokio::time::sleep(delay).await;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::wayland::{ReadyOutput, ReadyStream, StreamCursorMode};

    fn reply(generation: u64, node_id: u32, streams_unavailable: bool) -> OutputReply {
        OutputReply {
            ok: true,
            error: None,
            generation,
            outputs: vec![ReadyOutput {
                connector: "Meta-0".into(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }],
            streams: vec![ReadyStream {
                connector: "Meta-0".into(),
                node_id,
                width: 1920,
                height: 1080,
                x: 0,
                y: 0,
            }],
            streams_unavailable,
        }
    }

    #[tokio::test]
    async fn an_attachment_waits_for_a_locked_desktops_streams_to_be_recorded_again() {
        let runtime_dir =
            std::env::temp_dir().join(format!("arcen-output-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime_dir);
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let socket = runtime_dir.join("arcen-output.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // The agent answers from its snapshot: closed streams twice (the
        // desktop is still locked), then the next generation's new nodes.
        let agent = tokio::spawn(async move {
            let answers = [reply(1, 41, true), reply(1, 41, true), reply(2, 57, false)];
            for answer in answers {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut line = String::new();
                BufReader::new(reader).read_line(&mut line).await.unwrap();
                assert!(line.contains("hello"));
                let mut encoded = serde_json::to_vec(&answer).unwrap();
                encoded.push(b'\n');
                writer.write_all(&encoded).await.unwrap();
            }
        });
        let ready = WaylandAgentReady {
            compositor: "gnome-shell".into(),
            input_socket: runtime_dir.join("arcen-input.sock"),
            runtime_dir: runtime_dir.clone(),
            wayland_display: "wayland-0".into(),
            outputs: reply(1, 41, false).outputs,
            streams: reply(1, 41, false).streams,
            input_device_types: 7,
            generation: 1,
            compositor_render_node: None,
            compositor_gpu_pci: None,
            compositor_gpu_vendor: None,
            output_mode: arcen_session::wayland_outputs::VirtualOutputMode::RemoteMonitors,
            output_socket: Some(socket),
            x11: None,
            cursor_mode: StreamCursorMode::Embedded,
            clipboard: None,
        };
        let live = current_live(&ready, nix::unistd::getuid().as_raw(), std::process::id())
            .await
            .unwrap();
        assert_eq!(live.generation, 2);
        assert_eq!(live.streams[0].node_id, 57);
        assert!(!live.streams_unavailable);
        agent.await.unwrap();
        let _ = std::fs::remove_dir_all(&runtime_dir);
    }
}
