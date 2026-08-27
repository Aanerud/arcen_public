//! Dedicated-Xorg clipboard policy, bounded relay state, and user-agent supervision.

use arcen_media::clipboard::{
    ClipboardContent, ClipboardDirection, ClipboardFlow, ClipboardKind, ClipboardPolicy,
    HARD_MAX_CLIPBOARD_BYTES,
};
use arcen_protocol::messages::{
    ClientHelloMsg, ClipboardContentKind, ClipboardContentMsg, ClipboardDataMsg,
    ClipboardDirectionMsg, ClipboardPolicyMsg, CLIPBOARD_PROTOCOL_VERSION,
};
use arcen_protocol::{encode_clipboard_chunk, ClipboardChunkHeader, CHUNK_BYTES};
use arcen_telemetry::CorrelationId;
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Formatter};
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use zeroize::Zeroize;

use crate::cli::{AuthMode, Config};
use crate::session::identity::UserExecution;
use crate::session::lifecycle::SessionMetadata;

const IPC_READY_LIMIT: usize = 4096;
const IPC_READY_TIMEOUT: Duration = Duration::from_secs(5);
const IPC_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const CLIPBOARD_POLICY_TYPE: &str = "arcen_clipboard_policy";

/// Advertises disabled on no-auth/shared-display sessions and exact host policy
/// only for authenticated dedicated-Xorg sessions.
#[must_use]
pub fn advertised_policy(cfg: &Config, session: Option<&SessionMetadata>) -> ClipboardPolicyMsg {
    let eligible = cfg.auth_mode == AuthMode::Pam
        && session.is_some_and(|session| {
            session.session_type == "x11"
                && session.display == cfg.session_display
                && session.uid != 0
        });
    let mut policy = cfg.clipboard_policy;
    if !eligible {
        policy.direction = ClipboardDirection::Disabled;
    }
    policy_message(policy)
}

#[must_use]
pub fn policy_message(policy: ClipboardPolicy) -> ClipboardPolicyMsg {
    ClipboardPolicyMsg {
        protocol_version: CLIPBOARD_PROTOCOL_VERSION,
        direction: match policy.direction {
            ClipboardDirection::Both => ClipboardDirectionMsg::Both,
            ClipboardDirection::ClientToHost => ClipboardDirectionMsg::ClientToHost,
            ClipboardDirection::HostToClient => ClipboardDirectionMsg::HostToClient,
            ClipboardDirection::Disabled => ClipboardDirectionMsg::Disabled,
        },
        content: match policy.content {
            ClipboardContent::All => ClipboardContentMsg::All,
            ClipboardContent::Text => ClipboardContentMsg::Text,
            ClipboardContent::Image => ClipboardContentMsg::Image,
        },
        max_bytes: u32::try_from(policy.max_bytes)
            .expect("validated clipboard policy always fits u32"),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ClipboardNegotiation {
    policy: ClipboardPolicy,
    text_c2h: bool,
    text_h2c: bool,
    image_c2h: bool,
    image_h2c: bool,
}

impl ClipboardNegotiation {
    #[must_use]
    pub fn from_client(
        policy: ClipboardPolicy,
        eligible: bool,
        hello: &ClientHelloMsg,
    ) -> Option<Self> {
        if !eligible
            || hello.clipboard_protocol_version != CLIPBOARD_PROTOCOL_VERSION
            || matches!(policy.direction, ClipboardDirection::Disabled)
        {
            return None;
        }
        let value = Self {
            policy,
            text_c2h: hello.clipboard_text_c2s,
            text_h2c: hello.clipboard_text_s2c,
            image_c2h: hello.clipboard_image_c2s,
            image_h2c: hello.clipboard_image_s2c,
        };
        (value.allows(ClipboardFlow::ClientToHost, ClipboardContentKind::TextUtf8)
            || value.allows(ClipboardFlow::ClientToHost, ClipboardContentKind::ImagePng)
            || value.allows(ClipboardFlow::HostToClient, ClipboardContentKind::TextUtf8)
            || value.allows(ClipboardFlow::HostToClient, ClipboardContentKind::ImagePng))
        .then_some(value)
    }

    #[must_use]
    pub const fn policy(self) -> ClipboardPolicy {
        self.policy
    }

    #[must_use]
    pub fn allows(self, flow: ClipboardFlow, kind: ClipboardContentKind) -> bool {
        if !self.policy.allows(flow, media_kind(kind)) {
            return false;
        }
        match (flow, kind) {
            (ClipboardFlow::ClientToHost, ClipboardContentKind::TextUtf8) => self.text_c2h,
            (ClipboardFlow::HostToClient, ClipboardContentKind::TextUtf8) => self.text_h2c,
            (ClipboardFlow::ClientToHost, ClipboardContentKind::ImagePng) => self.image_c2h,
            (ClipboardFlow::HostToClient, ClipboardContentKind::ImagePng) => self.image_h2c,
        }
    }
}

fn media_kind(kind: ClipboardContentKind) -> ClipboardKind {
    match kind {
        ClipboardContentKind::TextUtf8 => ClipboardKind::TextUtf8,
        ClipboardContentKind::ImagePng => ClipboardKind::ImagePng,
    }
}

pub struct ClipboardItem {
    pub sequence: u64,
    pub kind: ClipboardContentKind,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl ClipboardItem {
    #[must_use]
    pub fn new(
        sequence: u64,
        kind: ClipboardContentKind,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> Option<Self> {
        if sequence == 0
            || bytes.is_empty()
            || bytes.len() > HARD_MAX_CLIPBOARD_BYTES
            || (truncated && kind != ClipboardContentKind::TextUtf8)
        {
            return None;
        }
        Some(Self {
            sequence,
            kind,
            bytes,
            truncated,
        })
    }

    fn scrub(&mut self) {
        self.bytes.zeroize();
        self.bytes.clear();
    }
}

impl Debug for ClipboardItem {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardItem")
            .field("sequence", &self.sequence)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl Drop for ClipboardItem {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

struct Transfer {
    item: ClipboardItem,
    offer_sent: bool,
    offset: usize,
}

impl Transfer {
    fn next_message(&mut self) -> Result<Message, String> {
        if !self.offer_sent {
            self.offer_sent = true;
            let offer = ClipboardDataMsg::new(
                self.item.sequence,
                self.item.kind,
                u32::try_from(self.item.bytes.len())
                    .map_err(|_| "clipboard payload size exceeds u32".to_string())?,
                self.item.truncated,
            );
            return serde_json::to_string(&offer)
                .map(|text| Message::Text(text.into()))
                .map_err(|error| format!("serialize clipboard offer: {error}"));
        }
        let end = self
            .offset
            .checked_add(CHUNK_BYTES)
            .unwrap_or(self.item.bytes.len())
            .min(self.item.bytes.len());
        let frame = encode_clipboard_chunk(
            ClipboardChunkHeader {
                kind: self.item.kind,
                sequence: self.item.sequence,
                total_size: u32::try_from(self.item.bytes.len())
                    .map_err(|_| "clipboard payload size exceeds u32".to_string())?,
                offset: u32::try_from(self.offset)
                    .map_err(|_| "clipboard offset exceeds u32".to_string())?,
            },
            &self.item.bytes[self.offset..end],
        )
        .map_err(|error| format!("encode clipboard frame: {error:?}"))?;
        self.offset = end;
        Ok(Message::Binary(frame.into()))
    }

    const fn finished(&self) -> bool {
        self.offer_sent && self.offset == self.item.bytes.len()
    }
}

#[derive(Default)]
struct WriterState {
    latest: u64,
    active: Option<Transfer>,
    pending: Option<ClipboardItem>,
    closed: bool,
}

impl Drop for WriterState {
    fn drop(&mut self) {
        self.active = None;
        if let Some(mut pending) = self.pending.take() {
            pending.scrub();
        }
    }
}

pub struct ClipboardWriterQueue {
    state: Mutex<WriterState>,
    ready: Notify,
}

impl ClipboardWriterQueue {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WriterState::default()),
            ready: Notify::new(),
        })
    }

    pub fn enqueue(&self, mut item: ClipboardItem) -> bool {
        let mut state = self.lock();
        if state.closed || item.sequence <= state.latest {
            item.scrub();
            return false;
        }
        state.latest = item.sequence;
        if let Some(mut pending) = state.pending.take() {
            pending.scrub();
        }
        state.pending = Some(item);
        drop(state);
        self.ready.notify_one();
        true
    }

    pub async fn pop(&self) -> Result<Option<Message>, String> {
        loop {
            {
                let mut state = self.lock();
                if state
                    .pending
                    .as_ref()
                    .zip(state.active.as_ref())
                    .is_some_and(|(pending, active)| pending.sequence > active.item.sequence)
                {
                    state.active = None;
                }
                if state.active.is_none() {
                    state.active = state.pending.take().map(|item| Transfer {
                        item,
                        offer_sent: false,
                        offset: 0,
                    });
                }
                if let Some(active) = state.active.as_mut() {
                    let message = active.next_message()?;
                    if active.finished() {
                        state.active = None;
                    }
                    return Ok(Some(message));
                }
                if state.closed {
                    return Ok(None);
                }
            }
            self.ready.notified().await;
        }
    }

    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.active = None;
        if let Some(mut pending) = state.pending.take() {
            pending.scrub();
        }
        drop(state);
        self.ready.notify_one();
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WriterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Default)]
struct MailboxState {
    latest: u64,
    item: Option<ClipboardItem>,
    closed: bool,
}

pub struct ClipboardMailbox {
    state: Mutex<MailboxState>,
    ready: Notify,
}

impl ClipboardMailbox {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MailboxState::default()),
            ready: Notify::new(),
        })
    }

    pub fn replace(&self, mut item: ClipboardItem) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || item.sequence <= state.latest {
            item.scrub();
            return false;
        }
        state.latest = item.sequence;
        if let Some(mut old) = state.item.take() {
            old.scrub();
        }
        state.item = Some(item);
        drop(state);
        self.ready.notify_one();
        true
    }

    pub async fn take(&self) -> Option<ClipboardItem> {
        loop {
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(item) = state.item.take() {
                    return Some(item);
                }
                if state.closed {
                    return None;
                }
            }
            self.ready.notified().await;
        }
    }

    pub fn try_take(&self) -> Option<ClipboardItem> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .item
            .take()
    }

    pub fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        if let Some(mut item) = state.item.take() {
            item.scrub();
        }
        drop(state);
        self.ready.notify_one();
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }
}

/// Pure one-direction ICCCM INCR state with a progress deadline.
#[derive(Debug)]
pub struct IncrTransfer {
    bytes: Vec<u8>,
    offset: usize,
    last_progress: Instant,
}

impl IncrTransfer {
    #[must_use]
    pub fn new(bytes: Vec<u8>, now: Instant) -> Option<Self> {
        (!bytes.is_empty() && bytes.len() <= HARD_MAX_CLIPBOARD_BYTES).then_some(Self {
            bytes,
            offset: 0,
            last_progress: now,
        })
    }

    pub fn next_chunk(&mut self, now: Instant) -> Option<&[u8]> {
        if self.offset == self.bytes.len() {
            return None;
        }
        let start = self.offset;
        let end = start
            .checked_add(CHUNK_BYTES)
            .unwrap_or(self.bytes.len())
            .min(self.bytes.len());
        self.offset = end;
        self.last_progress = now;
        Some(&self.bytes[start..end])
    }

    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_progress) >= Duration::from_secs(5)
    }
}

impl Drop for IncrTransfer {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardAgentReady {
    pub ready: bool,
    pub pid: u32,
    pub uid: u32,
    pub username: String,
    pub display: String,
    pub xfixes_major: u32,
    pub xfixes_minor: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct AgentPolicy {
    #[serde(rename = "type")]
    msg_type: String,
    policy: ClipboardPolicyMsg,
}

pub struct ClipboardAgentIo {
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl AsyncRead for ClipboardAgentIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buffer)
    }
}

impl AsyncWrite for ClipboardAgentIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.stdin).poll_write(cx, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

pub struct ClipboardAgentProcess {
    child: Child,
    stderr: JoinHandle<()>,
}

impl ClipboardAgentProcess {
    pub async fn shutdown(mut self) {
        terminate_child(&mut self.child).await;
        finish_stderr(&mut self.stderr).await;
    }
}

impl Drop for ClipboardAgentProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.stderr.abort();
    }
}

pub async fn spawn_clipboard_agent(
    binary: &Path,
    execution: &UserExecution,
    policy: ClipboardPolicy,
    session_log_id: &CorrelationId,
) -> Result<(ClipboardAgentProcess, WebSocketStream<ClipboardAgentIo>), String> {
    let mut command = crate::command_for_helper(binary, "session-agent");
    command
        .arg("--clipboard-agent")
        .arg("--session-log-id")
        .arg(session_log_id.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    execution
        .configure(&mut command)
        .map_err(|error| format!("clipboard agent identity: {error}"))?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn clipboard agent: {error}"))?;
    let pid = child
        .id()
        .ok_or_else(|| "clipboard agent exited before readiness".to_string())?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "clipboard agent stdin unavailable".to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "clipboard agent stdout unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "clipboard agent stderr unavailable".to_string())?;
    let stderr = tokio::spawn(read_stderr(stderr));
    let line = tokio::time::timeout(IPC_READY_TIMEOUT, read_ready_line(&mut stdout))
        .await
        .map_err(|_| "clipboard agent readiness timed out".to_string())??;
    let ready: ClipboardAgentReady = serde_json::from_slice(&line)
        .map_err(|_| "clipboard agent READY is invalid".to_string())?;
    if !ready.ready
        || ready.pid != pid
        || ready.uid != execution.identity.uid
        || ready.username != execution.identity.username
        || ready.display != execution.environment.display()
        || ready.xfixes_major < 5
    {
        return Err("clipboard agent READY identity/XFixes mismatch".to_string());
    }
    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(arcen_protocol::CLIPBOARD_HEADER_SIZE + CHUNK_BYTES);
    config.max_frame_size = Some(arcen_protocol::CLIPBOARD_HEADER_SIZE + CHUNK_BYTES);
    let io = ClipboardAgentIo { stdin, stdout };
    let mut websocket = WebSocketStream::from_raw_socket(io, Role::Client, Some(config)).await;
    let policy = AgentPolicy {
        msg_type: CLIPBOARD_POLICY_TYPE.to_string(),
        policy: policy_message(policy),
    };
    websocket
        .send(Message::Text(
            serde_json::to_string(&policy)
                .map_err(|error| format!("serialize clipboard child policy: {error}"))?
                .into(),
        ))
        .await
        .map_err(|error| format!("send clipboard child policy: {error}"))?;
    Ok((ClipboardAgentProcess { child, stderr }, websocket))
}

async fn read_ready_line(stdout: &mut ChildStdout) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    while line.len() < IPC_READY_LIMIT {
        let byte = stdout
            .read_u8()
            .await
            .map_err(|error| format!("read clipboard READY: {error}"))?;
        if byte == b'\n' {
            return Ok(line);
        }
        line.push(byte);
    }
    Err("clipboard READY exceeds bound".to_string())
}

async fn read_stderr(stderr: ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(
            target: crate::logging::target::SESSION,
            clipboard_agent = true,
            "{line}"
        );
    }
}

async fn finish_stderr(task: &mut JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(1), &mut *task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

async fn terminate_child(child: &mut Child) {
    #[cfg(target_os = "linux")]
    if let Some(pid) = child.id() {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX)),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    if tokio::time::timeout(IPC_SHUTDOWN_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        #[cfg(target_os = "linux")]
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX)),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill().await;
    }
}

#[cfg(target_os = "linux")]
pub async fn run_clipboard_child() -> Result<(), String> {
    native::run().await
}

#[cfg(target_os = "linux")]
fn effective_uid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

#[cfg(not(target_os = "linux"))]
pub async fn run_clipboard_child() -> Result<(), String> {
    Err("clipboard agent is available only on Linux".to_string())
}

#[cfg(target_os = "linux")]
mod native;

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ClipboardPolicy {
        ClipboardPolicy::new(
            ClipboardDirection::Both,
            ClipboardContent::All,
            2 * 1024 * 1024,
        )
        .unwrap()
    }

    #[test]
    fn no_auth_and_shared_display_advertise_disabled() {
        let cfg = Config::default();
        assert_eq!(
            advertised_policy(&cfg, None).direction,
            ClipboardDirectionMsg::Disabled
        );
    }

    #[test]
    fn negotiation_requires_exact_v1() {
        let mut hello = ClientHelloMsg {
            clipboard_protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            clipboard_text_c2s: true,
            ..ClientHelloMsg::default()
        };
        assert!(ClipboardNegotiation::from_client(policy(), true, &hello).is_some());
        hello.clipboard_protocol_version = 0;
        assert!(ClipboardNegotiation::from_client(policy(), true, &hello).is_none());
        assert!(ClipboardNegotiation::from_client(policy(), false, &hello).is_none());
    }

    #[test]
    fn incr_chunks_and_deadline_are_bounded() {
        let start = Instant::now();
        let mut incr = IncrTransfer::new(vec![7; CHUNK_BYTES + 1], start).unwrap();
        assert_eq!(incr.next_chunk(start).unwrap().len(), CHUNK_BYTES);
        assert_eq!(incr.next_chunk(start).unwrap().len(), 1);
        assert!(incr.next_chunk(start).is_none());
        assert!(!incr.expired(start + Duration::from_millis(4_999)));
        assert!(incr.expired(start + Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn writer_is_latest_bounded_and_schedules_one_chunk() {
        let queue = ClipboardWriterQueue::new();
        assert!(queue.enqueue(
            ClipboardItem::new(
                1,
                ClipboardContentKind::TextUtf8,
                vec![b'a'; CHUNK_BYTES + 1],
                false,
            )
            .unwrap()
        ));
        assert!(matches!(queue.pop().await.unwrap(), Some(Message::Text(_))));
        assert!(queue.enqueue(
            ClipboardItem::new(2, ClipboardContentKind::TextUtf8, b"new".to_vec(), false).unwrap()
        ));
        assert!(matches!(queue.pop().await.unwrap(), Some(Message::Text(_))));
        let Some(Message::Binary(frame)) = queue.pop().await.unwrap() else {
            panic!("new payload chunk");
        };
        assert_eq!(frame.len(), arcen_protocol::CLIPBOARD_HEADER_SIZE + 3);
    }
}
