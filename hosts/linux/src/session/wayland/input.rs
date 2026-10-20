//! Session-bound input socket of a headless Wayland session.
//!
//! Newline-delimited JSON [`InputCommand`]s on
//! `$XDG_RUNTIME_DIR/arcen-input.sock` inside the session's private runtime
//! directory (mode 0700, owned by the session user). Only root (the Pier)
//! and the session user may connect. Every command is validated against the
//! session's own streams and output generation and injected through a
//! [`CompositorInput`] — today the session's Mutter `RemoteDesktop` session
//! — so it can only reach this compositor.
//!
//! One connection is served at a time. A newer connection supersedes the
//! current one: what the old one still held is released first, and the old
//! one is closed, so a stale Pier attachment can never inject after its
//! successor connected. Whatever a connection still holds when it ends — or
//! when the session ends — is released.
//!
//! [`CompositorInput`] is the seam an EIS (libei, `ConnectToEIS`) injector
//! replaces: the socket protocol, validation and held-input release stay as
//! they are.

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arcen_input::text::TextRun;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use super::text::SessionText;
use super::{
    monotonic_now_ns, validate_input, HeldInput, InputCommand, InputReply, InputRequest,
    InputSessionFacts, LockState, RememberedNumLock, EXPIRED_REASON, LOCK_KEYCODES,
    SUPERSEDED_REASON,
};
use crate::bounded_io::BoundedLineReader;
use crate::logging::target;

const MAX_COMMAND_BYTES: usize = 4096;
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const NOTICE_TIMEOUT: Duration = Duration::from_millis(200);

/// The compositor interface commands are injected through.
pub trait CompositorInput: Sync {
    /// Why the compositor refused a command.
    type Error: std::fmt::Display + Send;

    /// Injects one validated command. Never called with
    /// [`InputCommand::Hello`], [`InputCommand::ReleaseAll`],
    /// [`InputCommand::Locks`], [`InputCommand::Text`] or
    /// [`InputCommand::Keymap`].
    fn notify(
        &self,
        command: &InputCommand,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// The facts a `hello` reports. Output reconfiguration advances them: the
/// generation and streams every coordinate is checked against.
#[derive(Debug)]
pub struct SessionFacts(std::sync::RwLock<InputSessionFacts>);

impl SessionFacts {
    /// Facts of a session's first output set.
    #[must_use]
    pub const fn new(facts: InputSessionFacts) -> Self {
        Self(std::sync::RwLock::new(facts))
    }

    /// The current facts.
    #[must_use]
    pub fn snapshot(&self) -> InputSessionFacts {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Moves to output generation `generation` with `streams`.
    pub fn advance(&self, generation: u64, streams: Vec<super::ReadyStream>) {
        let mut facts = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.generation = generation;
        facts.streams = streams;
    }
}

impl From<InputSessionFacts> for SessionFacts {
    fn from(facts: InputSessionFacts) -> Self {
        Self::new(facts)
    }
}

/// Reads GNOME's remembered Num Lock in the session (`gsettings`, run as
/// the session user with its bus).
#[must_use]
pub fn read_remembered_num_lock() -> RememberedNumLock {
    let read = |key: &str| -> Option<bool> {
        let output = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.peripherals.keyboard", key])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| parse_gsettings_bool(&String::from_utf8_lossy(&output.stdout)))?
    };
    RememberedNumLock {
        remember: read("remember-numlock-state"),
        state: read("numlock-state"),
    }
}

fn parse_gsettings_bool(printed: &str) -> Option<bool> {
    match printed.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The input socket and what it holds.
pub struct InputService {
    listener: UnixListener,
    held: Mutex<HeldInput>,
    locks: Mutex<LockState>,
    uid: u32,
    text: Arc<SessionText>,
}

struct Connection {
    reader: BoundedLineReader<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    number: u64,
}

impl InputService {
    /// Binds the socket at `path`, readable and writable by its owner only.
    ///
    /// # Errors
    ///
    /// When the socket cannot be created.
    pub fn bind(path: &Path, uid: u32) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            held: Mutex::new(HeldInput::default()),
            locks: Mutex::new(LockState::default()),
            uid,
            text: Arc::new(SessionText::default()),
        })
    }

    /// Types committed text with `text`'s knowledge of the session's layout.
    #[must_use]
    pub fn with_text(mut self, text: Arc<SessionText>) -> Self {
        self.text = text;
        self
    }

    /// Starts lock tracking from the state the session's compositor
    /// started with, instead of every lock off.
    #[must_use]
    pub fn with_locks(mut self, locks: LockState) -> Self {
        self.locks = Mutex::new(locks);
        self
    }

    /// Accepts the next connection from root or the session user; refuses
    /// and closes any other.
    async fn accept(&self, number: &mut u64) -> Connection {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else {
                continue;
            };
            let peer = stream.peer_cred().ok().map(|cred| cred.uid());
            if !matches!(peer, Some(uid) if uid == 0 || uid == self.uid) {
                tracing::warn!(
                    target: target::INPUT,
                    peer_uid = ?peer,
                    "refused an input connection from another user"
                );
                continue;
            }
            *number += 1;
            return connection(stream, *number);
        }
    }

    /// Serves connections until the future is dropped. `session` is what a
    /// `hello` reports; its streams and generation bound every coordinate.
    pub async fn serve<B: CompositorInput>(&self, backend: &B, session: &SessionFacts) {
        let mut number = 0;
        let mut current = self.accept(&mut number).await;
        tracing::info!(target: target::INPUT, connection = current.number, "input connection accepted");
        loop {
            // Only reading races a newer connection; a command being handled
            // is never cancelled half-way. A waiting connection wins: it
            // supersedes the current one before the current one's leftover
            // lines are read, so a reconnecting sender's abandoned commands
            // are never injected.
            let line = tokio::select! {
                biased;
                next = self.accept(&mut number) => {
                    tracing::info!(
                        target: target::INPUT,
                        superseded = current.number,
                        connection = next.number,
                        "a newer input connection superseded the current one"
                    );
                    // Told, so it does not reconnect and take the input back.
                    let mut notice =
                        serde_json::to_vec(&InputReply::refused(SUPERSEDED_REASON)).unwrap_or_default();
                    notice.push(b'\n');
                    let _ = tokio::time::timeout(NOTICE_TIMEOUT, current.writer.write_all(&notice)).await;
                    self.release_held(backend).await;
                    current = next;
                    continue;
                }
                line = current.reader.next_bounded_line() => line,
            };
            let reply = match line {
                Ok(Some(line)) if line.truncated || line.text.len() > MAX_COMMAND_BYTES => {
                    InputReply::refused("command too long")
                }
                Ok(Some(line)) => match serde_json::from_str::<InputRequest>(&line.text) {
                    Ok(request) if request.expired_at(monotonic_now_ns()) => {
                        tracing::info!(
                            target: target::INPUT,
                            connection = current.number,
                            "refused an input command read after its deadline"
                        );
                        InputReply::refused(EXPIRED_REASON)
                    }
                    Ok(request) => {
                        self.handle(request.command, backend, session, current.number)
                            .await
                    }
                    Err(_) => InputReply::refused("malformed command"),
                },
                Ok(None) | Err(_) => {
                    tracing::info!(target: target::INPUT, connection = current.number, "input connection ended");
                    self.release_held(backend).await;
                    current = self.accept(&mut number).await;
                    tracing::info!(target: target::INPUT, connection = current.number, "input connection accepted");
                    continue;
                }
            };
            let reply = InputReply {
                keymap_generation: Some(self.text.generation()),
                ..reply
            };
            let mut encoded = serde_json::to_vec(&reply).unwrap_or_default();
            encoded.push(b'\n');
            let written =
                tokio::time::timeout(REPLY_TIMEOUT, current.writer.write_all(&encoded)).await;
            if !matches!(written, Ok(Ok(()))) {
                tracing::warn!(target: target::INPUT, connection = current.number, "input reply could not be written; closing the connection");
                self.release_held(backend).await;
                current = self.accept(&mut number).await;
            }
        }
    }

    async fn handle<B: CompositorInput>(
        &self,
        command: InputCommand,
        backend: &B,
        session: &SessionFacts,
        number: u64,
    ) -> InputReply {
        let session = &session.snapshot();
        if let Err(refused) = validate_input(&command, &session.streams, session.generation) {
            return InputReply::refused(&refused.to_string());
        }
        match command {
            InputCommand::Hello => {
                return InputReply {
                    session: Some(InputSessionFacts {
                        connection: number,
                        text: Some(self.text.facts()),
                        ..session.clone()
                    }),
                    ..InputReply::accepted()
                }
            }
            InputCommand::Keymap => {
                return InputReply {
                    keymap: self.text.session_keymap(),
                    ..InputReply::accepted()
                }
            }
            InputCommand::Text { text, .. } => return self.type_text(&text, backend).await,
            InputCommand::ReleaseAll => {
                self.release_held(backend).await;
                return InputReply::accepted();
            }
            InputCommand::Locks {
                caps_lock,
                num_lock,
                scroll_lock,
            } => {
                let toggles = self
                    .locks
                    .lock()
                    .await
                    .plan([caps_lock, num_lock, scroll_lock]);
                for toggle in toggles {
                    if let Err(error) = self.inject(toggle, backend).await {
                        return InputReply::refused(&error);
                    }
                }
                return InputReply::accepted();
            }
            _ => {}
        }
        match self.inject(command, backend).await {
            Ok(()) => InputReply::accepted(),
            Err(error) => InputReply::refused(&error),
        }
    }

    /// Types one commit: held modifiers released and Caps Lock turned off
    /// while keysyms are typed (the compositor would otherwise type other
    /// characters or shortcuts), then restored.
    async fn type_text<B: CompositorInput>(&self, text: &str, backend: &B) -> InputReply {
        let (plan, mut outcome) = self.text.plan(text);
        let mut released = Vec::new();
        let mut caps_toggled = false;
        let mut failure = None;
        if plan.types_keysyms() {
            let held = self.held.lock().await.held_modifiers();
            for keycode in held {
                match self
                    .inject(
                        InputCommand::Key {
                            keycode,
                            pressed: false,
                        },
                        backend,
                    )
                    .await
                {
                    Ok(()) => released.push(keycode),
                    Err(error) => failure = Some(error),
                }
            }
            if plan.caps_sensitive && self.locks.lock().await.states()[0] {
                caps_toggled = self.toggle_caps(backend).await.is_ok();
            }
        }
        if failure.is_none() {
            'runs: for run in &plan.runs {
                match run {
                    TextRun::Keysyms(keysyms) => {
                        for &keysym in keysyms {
                            for pressed in [true, false] {
                                if let Err(error) = self
                                    .inject(InputCommand::Keysym { keysym, pressed }, backend)
                                    .await
                                {
                                    failure = Some(error);
                                    break 'runs;
                                }
                            }
                        }
                    }
                    TextRun::Utf8(chunks) => {
                        for chunk in chunks {
                            if let Err(error) = self.text.commit_utf8(chunk) {
                                failure = Some(error);
                                break 'runs;
                            }
                        }
                    }
                }
            }
        }
        if caps_toggled {
            let _ = self.toggle_caps(backend).await;
        }
        for keycode in released {
            let _ = self
                .inject(
                    InputCommand::Key {
                        keycode,
                        pressed: true,
                    },
                    backend,
                )
                .await;
        }
        tracing::debug!(
            target: target::INPUT,
            keymap_chars = outcome.keymap_chars,
            unverified_chars = outcome.unverified_chars,
            utf8_chars = outcome.utf8_chars,
            refused_chars = outcome.refused_chars,
            reason = ?outcome.reason,
            group = outcome.group,
            "committed text typed"
        );
        if let Some(error) = failure {
            outcome.reason = Some(arcen_protocol::messages::TextRefusalReason::InputUnavailable);
            return InputReply {
                text: Some(outcome),
                ..InputReply::refused(&error)
            };
        }
        InputReply {
            text: Some(outcome),
            ..InputReply::accepted()
        }
    }

    async fn toggle_caps<B: CompositorInput>(&self, backend: &B) -> Result<(), String> {
        for pressed in [true, false] {
            self.inject(
                InputCommand::Key {
                    keycode: LOCK_KEYCODES[0],
                    pressed,
                },
                backend,
            )
            .await?;
        }
        Ok(())
    }

    async fn inject<B: CompositorInput>(
        &self,
        command: InputCommand,
        backend: &B,
    ) -> Result<(), String> {
        self.held.lock().await.apply_before(&command);
        backend
            .notify(&command)
            .await
            .map_err(|error| error.to_string())?;
        self.held.lock().await.apply(&command);
        self.locks.lock().await.apply_delivered(&command);
        Ok(())
    }

    /// Releases every key, button and touch still held.
    pub async fn release_held<B: CompositorInput>(&self, backend: &B) {
        let releases = self.held.lock().await.take_releases();
        if releases.is_empty() {
            return;
        }
        tracing::info!(
            target: target::INPUT,
            count = releases.len(),
            "releasing held Wayland input"
        );
        for release in releases {
            if let Err(error) = backend.notify(&release).await {
                tracing::warn!(target: target::INPUT, %error, "held input release failed");
            }
        }
    }
}

fn connection(stream: UnixStream, number: u64) -> Connection {
    let (reader, writer) = stream.into_split();
    Connection {
        reader: BoundedLineReader::new(BufReader::new(reader)),
        writer,
        number,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    use super::*;
    use crate::session::wayland::{InputBackendKind, ReadyStream};

    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<InputCommand>>);

    impl CompositorInput for Recording {
        type Error = std::convert::Infallible;

        fn notify(
            &self,
            command: &InputCommand,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.0.lock().unwrap().push(command.clone());
            std::future::ready(Ok(()))
        }
    }

    fn facts() -> InputSessionFacts {
        InputSessionFacts {
            generation: 3,
            streams: vec![ReadyStream {
                connector: "Meta-0".into(),
                node_id: 44,
                width: 1920,
                height: 1080,
                x: 0,
                y: 0,
            }],
            device_types: 7,
            remote_desktop_started: true,
            backend: InputBackendKind::RemoteDesktopDbus,
            eis_offered: None,
            stylus: None,
            connection: 0,
            text: None,
        }
    }

    struct Client {
        lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
        writer: OwnedWriteHalf,
    }

    impl Client {
        async fn connect(path: &Path) -> Self {
            let (reader, writer) = UnixStream::connect(path).await.unwrap().into_split();
            Self {
                lines: BufReader::new(reader).lines(),
                writer,
            }
        }

        async fn send(&mut self, command: &InputCommand) -> Option<InputReply> {
            self.send_request(&InputRequest::new(command.clone(), Duration::from_secs(5)))
                .await
        }

        async fn send_request(&mut self, request: &InputRequest) -> Option<InputReply> {
            let mut line = serde_json::to_vec(request).unwrap();
            line.push(b'\n');
            self.writer.write_all(&line).await.ok()?;
            let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
                .await
                .ok()?
                .ok()??;
            serde_json::from_str(&line).ok()
        }
    }

    #[tokio::test]
    async fn a_newer_connection_supersedes_and_releases_the_older_one() {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-input-service-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("arcen-input.sock");
        let uid = std::fs::metadata(&dir).map(|metadata| {
            use std::os::unix::fs::MetadataExt;
            metadata.uid()
        });
        let service = Arc::new(InputService::bind(&path, uid.unwrap()).unwrap());
        let backend = Arc::new(Recording::default());
        let serving = tokio::spawn({
            let service = Arc::clone(&service);
            let backend = Arc::clone(&backend);
            async move { service.serve(&*backend, &facts().into()).await }
        });

        let mut old = Client::connect(&path).await;
        let hello = old.send(&InputCommand::Hello).await.unwrap();
        assert_eq!(
            hello.session.as_ref().map(|facts| facts.connection),
            Some(1)
        );
        assert!(
            old.send(&InputCommand::Key {
                keycode: 30,
                pressed: true
            })
            .await
            .unwrap()
            .ok
        );
        let stale = old
            .send(&InputCommand::PointerAbsolute {
                output: "Meta-0".into(),
                generation: 2,
                x: 1.0,
                y: 1.0,
            })
            .await
            .unwrap();
        assert_eq!(stale.error.as_deref(), Some("stale output generation"));

        let mut new = Client::connect(&path).await;
        let hello = new.send(&InputCommand::Hello).await.unwrap();
        assert_eq!(hello.session.map(|facts| facts.connection), Some(2));
        assert!(
            backend.0.lock().unwrap().contains(&InputCommand::Key {
                keycode: 30,
                pressed: false
            }),
            "the superseded connection's held key is released"
        );
        let notice: InputReply = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(5), old.lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            notice.error.as_deref(),
            Some(SUPERSEDED_REASON),
            "the superseded connection is told before it is closed"
        );
        assert!(
            old.send(&InputCommand::Key {
                keycode: 31,
                pressed: true
            })
            .await
            .is_none(),
            "the superseded connection is closed"
        );
        assert!(!backend.0.lock().unwrap().contains(&InputCommand::Key {
            keycode: 31,
            pressed: true
        }));

        // A connection that ends releases what it held.
        assert!(
            new.send(&InputCommand::PointerButton {
                button: 272,
                pressed: true
            })
            .await
            .unwrap()
            .ok
        );
        drop(new);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !backend
                .0
                .lock()
                .unwrap()
                .contains(&InputCommand::PointerButton {
                    button: 272,
                    pressed: false,
                })
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("released on disconnect");
        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_session_started_with_num_lock_restored_turns_it_off_for_the_deck() {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-input-numlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("arcen-input.sock");
        let uid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir).unwrap().uid()
        };
        // GNOME's default remembers Num Lock; Mutter restored it on.
        let restored = LockState::at_session_start(RememberedNumLock {
            remember: Some(true),
            state: Some(true),
        });
        let service = Arc::new(InputService::bind(&path, uid).unwrap().with_locks(restored));
        let backend = Arc::new(Recording::default());
        let serving = tokio::spawn({
            let service = Arc::clone(&service);
            let backend = Arc::clone(&backend);
            async move { service.serve(&*backend, &facts().into()).await }
        });
        let mut client = Client::connect(&path).await;
        let num_lock = |on| InputCommand::Locks {
            caps_lock: Some(false),
            num_lock: Some(on),
            scroll_lock: None,
        };
        assert!(client.send(&num_lock(false)).await.unwrap().ok);
        assert!(client.send(&num_lock(false)).await.unwrap().ok);
        assert_eq!(
            *backend.0.lock().unwrap(),
            [
                InputCommand::Key {
                    keycode: 69,
                    pressed: true
                },
                InputCommand::Key {
                    keycode: 69,
                    pressed: false
                },
            ],
            "one Num Lock toggle, and no Caps Lock"
        );
        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gsettings_booleans_are_read_strictly() {
        assert_eq!(parse_gsettings_bool("true\n"), Some(true));
        assert_eq!(parse_gsettings_bool("false"), Some(false));
        assert_eq!(parse_gsettings_bool("No such schema"), None);
    }

    #[tokio::test]
    async fn the_session_reconciles_locks_against_what_it_delivered() {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-input-locks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("arcen-input.sock");
        let uid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir).unwrap().uid()
        };
        let service = Arc::new(InputService::bind(&path, uid).unwrap());
        let backend = Arc::new(Recording::default());
        let serving = tokio::spawn({
            let service = Arc::clone(&service);
            let backend = Arc::clone(&backend);
            async move { service.serve(&*backend, &facts().into()).await }
        });
        let mut client = Client::connect(&path).await;
        let locks = |caps_lock| InputCommand::Locks {
            caps_lock,
            num_lock: Some(false),
            scroll_lock: None,
        };
        assert!(client.send(&locks(Some(true))).await.unwrap().ok);
        // Already on: asking again toggles nothing.
        assert!(client.send(&locks(Some(true))).await.unwrap().ok);
        // A Caps Lock key typed by the Deck toggles it too.
        for pressed in [true, false] {
            client
                .send(&InputCommand::Key {
                    keycode: 58,
                    pressed,
                })
                .await
                .unwrap();
        }
        assert!(client.send(&locks(Some(true))).await.unwrap().ok);
        let caps = backend
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    InputCommand::Key {
                        keycode: 58,
                        pressed: true
                    }
                )
            })
            .count();
        assert_eq!(caps, 3, "on, off by the typed key, on again");
        assert!(!backend
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|command| matches!(command, InputCommand::Key { keycode: 69, .. })));
        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_expired_command_is_refused_and_never_injected() {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-input-expired-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("arcen-input.sock");
        let uid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir).unwrap().uid()
        };
        let service = Arc::new(InputService::bind(&path, uid).unwrap());
        let backend = Arc::new(Recording::default());
        let serving = tokio::spawn({
            let service = Arc::clone(&service);
            let backend = Arc::clone(&backend);
            async move { service.serve(&*backend, &facts().into()).await }
        });
        let mut client = Client::connect(&path).await;
        let press = InputCommand::Key {
            keycode: 30,
            pressed: true,
        };
        let stale = InputRequest {
            not_after_ns: monotonic_now_ns().saturating_sub(1),
            command: press.clone(),
        };
        let reply = client.send_request(&stale).await.unwrap();
        assert_eq!(reply.error.as_deref(), Some(EXPIRED_REASON));
        assert!(backend.0.lock().unwrap().is_empty(), "nothing injected");
        // A bare command without a deadline is not accepted either.
        let mut line = serde_json::to_vec(&press).unwrap();
        line.push(b'\n');
        client.writer.write_all(&line).await.unwrap();
        let reply: InputReply =
            serde_json::from_str(&client.lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reply.error.as_deref(), Some("malformed command"));
        assert!(client.send(&press).await.unwrap().ok);
        assert_eq!(backend.0.lock().unwrap().as_slice(), &[press]);
        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_hello_reports_the_text_route_a_background_task_set() {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-input-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("arcen-input.sock");
        let uid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir).unwrap().uid()
        };
        let text = Arc::new(SessionText::default());
        let service = Arc::new(
            InputService::bind(&path, uid)
                .unwrap()
                .with_text(Arc::clone(&text)),
        );
        let route = tokio::spawn({
            let text = Arc::clone(&text);
            async move {
                tokio::time::sleep(Duration::ZERO).await;
                text.set_eis_error(Some("connecting".to_owned()));
            }
        });
        let backend = Arc::new(Recording::default());
        let serving = tokio::spawn({
            let service = Arc::clone(&service);
            let backend = Arc::clone(&backend);
            async move { service.serve(&*backend, &facts().into()).await }
        });
        let mut client = Client::connect(&path).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let hello = client.send(&InputCommand::Hello).await.unwrap();
        assert_eq!(
            hello.session.unwrap().text.unwrap().eis_error.as_deref(),
            Some("connecting")
        );
        route.await.unwrap();
        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn committed_text_is_typed_with_modifiers_and_caps_lock_neutral() {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-input-text-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("arcen-input.sock");
        let uid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir).unwrap().uid()
        };
        let service = Arc::new(InputService::bind(&path, uid).unwrap());
        let backend = Arc::new(Recording::default());
        let serving = tokio::spawn({
            let service = Arc::clone(&service);
            let backend = Arc::clone(&backend);
            async move { service.serve(&*backend, &facts().into()).await }
        });
        let mut client = Client::connect(&path).await;
        let hello = client.send(&InputCommand::Hello).await.unwrap();
        let text_facts = hello.session.unwrap().text.unwrap();
        assert!(!text_facts.keymap_known);
        assert_eq!(hello.keymap_generation, Some(0));
        let shift = InputCommand::Key {
            keycode: 42,
            pressed: true,
        };
        assert!(client.send(&shift).await.unwrap().ok);
        let caps_on = InputCommand::Locks {
            caps_lock: Some(true),
            num_lock: None,
            scroll_lock: None,
        };
        assert!(client.send(&caps_on).await.unwrap().ok);
        backend.0.lock().unwrap().clear();

        let reply = client
            .send(&InputCommand::Text {
                text: "a".to_owned(),
                sequence: 3,
            })
            .await
            .unwrap();
        assert!(reply.ok);
        let outcome = reply.text.unwrap();
        // No keymap read: typed, but unproven.
        assert_eq!(outcome.unverified_chars, 1);
        let key = |keycode, pressed| InputCommand::Key { keycode, pressed };
        let keysym = |pressed| InputCommand::Keysym {
            keysym: 0x61,
            pressed,
        };
        assert_eq!(
            backend.0.lock().unwrap().as_slice(),
            &[
                key(42, false),
                key(58, true),
                key(58, false),
                keysym(true),
                keysym(false),
                key(58, true),
                key(58, false),
                key(42, true),
            ]
        );
        let refused = client
            .send(&InputCommand::Text {
                text: "a\nb".to_owned(),
                sequence: 4,
            })
            .await
            .unwrap();
        assert_eq!(refused.error.as_deref(), Some("invalid committed text"));
        let layout = client.send(&InputCommand::Keymap).await.unwrap();
        assert!(layout.ok && layout.keymap.is_none());
        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_waiting_connection_supersedes_before_leftover_lines_are_read() {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-input-leftover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("arcen-input.sock");
        let uid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir).unwrap().uid()
        };
        let service = Arc::new(InputService::bind(&path, uid).unwrap());
        let backend = Arc::new(Recording::default());
        // A stalled session: lines and a new connection are both waiting
        // before it serves again.
        let mut old = Client::connect(&path).await;
        let stale = InputRequest::new(
            InputCommand::Key {
                keycode: 36,
                pressed: true,
            },
            Duration::from_secs(60),
        );
        let mut line = serde_json::to_vec(&stale).unwrap();
        line.push(b'\n');
        let serving = tokio::spawn({
            let service = Arc::clone(&service);
            let backend = Arc::clone(&backend);
            async move { service.serve(&*backend, &facts().into()).await }
        });
        // The first connection is accepted at once; give it its leftover
        // line and a waiting successor together.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut new = Client::connect(&path).await;
        old.writer.write_all(&line).await.unwrap();
        let hello = new.send(&InputCommand::Hello).await.unwrap();
        assert_eq!(hello.session.map(|facts| facts.connection), Some(2));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !backend.0.lock().unwrap().contains(&stale.command),
            "the superseded connection's leftover line is never read"
        );
        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
