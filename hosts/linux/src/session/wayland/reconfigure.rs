//! Mid-session output reconfiguration of a remote-monitor session: resize,
//! re-placement and topology change as one [`arcen_outputs::OutputTransaction`],
//! served to the Pier on the session-bound output socket.
//!
//! | Stage | Resize / move (same count) | Topology change (other count) |
//! | --- | --- | --- |
//! | preflight | the next generation; a layout the mode serves | same |
//! | bind | renegotiate each resized stream; place every output | a new paired session with one `RecordVirtual` stream per output, negotiated and placed; the live outputs stay |
//! | verify | `DisplayConfig` reports exactly the requested geometry and placement | same, for the new set |
//! | commit | — | the new set becomes live; the old session is stopped, its monitors vanish |
//! | rollback | renegotiate and place the previous outputs | stop the new session; place the live outputs again |
//!
//! The Pier stops every capture epoch of the session first: a stream is
//! renegotiated only while nothing else consumes it. Input is held off for
//! the transaction (the live set is write-locked) and anything held is
//! released before it starts.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arcen_outputs::{
    BindFailure, OutputCapabilities, OutputContext, OutputDemand, OutputProvider, OutputTransaction,
};
use arcen_session::wayland_outputs::{
    check_next_generation, plan_reconfiguration, PlacedOutput, Reconfiguration, ReconfigureError,
    VirtualOutputMode,
};
use arcen_session::wayland_session::VerifiedOutput;
use arcen_telemetry::CorrelationId;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{watch, RwLock};
use zbus::Connection;

use super::input::{CompositorInput, InputService, SessionFacts};
use super::mutter::{MonitorCasts, MutterError};
use super::output::capabilities_for;
use super::remote::{self, RemoteOutputError, RemoteSet};
use super::stack::CompositorColour;
use super::{InputCommand, OutputCommand, OutputPlacement, OutputReply, ReadyOutput, ReadyStream};
use crate::bounded_io::BoundedLineReader;
use crate::logging::target;

const MAX_COMMAND_BYTES: usize = 4096;
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// A connection that sends nothing for this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Output connections served at once.
const MAX_CONNECTIONS: usize = 4;

/// A reconfiguration failure.
#[derive(Debug, Error)]
pub enum ReconfigureFailure {
    #[error("{0}")]
    Refused(#[from] ReconfigureError),
    #[error("outputs failed: {0}")]
    Outputs(#[from] RemoteOutputError),
    #[error("{0}")]
    Transaction(String),
}

/// The session's live outputs, shared by input injection and
/// reconfiguration.
#[derive(Clone)]
pub struct LiveOutputs(pub Arc<RwLock<RemoteSet>>);

impl CompositorInput for LiveOutputs {
    type Error = MutterError;

    fn notify(
        &self,
        command: &InputCommand,
    ) -> impl std::future::Future<Output = Result<(), MutterError>> + Send {
        let live = Arc::clone(&self.0);
        let command = command.clone();
        // Never wait for a reconfiguration: a command validated against the
        // outgoing generation would otherwise be injected into the next one
        // after its deadline, and the input loop would stall behind the
        // transaction. It is refused at once instead.
        async move {
            let set = unless_reconfiguring(&live)?;
            set.sessions.notify(&command).await
        }
    }
}

/// Read access to the live outputs, or [`MutterError::Reconfiguring`] at
/// once while a transaction holds them.
fn unless_reconfiguring<T>(
    live: &RwLock<T>,
) -> Result<tokio::sync::RwLockReadGuard<'_, T>, MutterError> {
    live.try_read().map_err(|_| MutterError::Reconfiguring)
}

/// Reconfigures the live set within one output transaction.
struct Reconfigure<'live> {
    connection: &'live Connection,
    runtime_dir: &'live Path,
    mode: VirtualOutputMode,
    live: &'live mut RemoteSet,
}

struct Prepared {
    change: Reconfiguration,
    requested: Vec<PlacedOutput>,
}

struct ReconfigureBinding {
    change: Reconfiguration,
    previous: Vec<PlacedOutput>,
    replacement: Option<RemoteSet>,
    /// The count changed on the running sessions (streams added or
    /// stopped) rather than through a replacement set.
    count_changed_in_place: bool,
    evidence: Vec<VerifiedOutput>,
    armed: bool,
}

impl Reconfigure<'_> {
    async fn restore(&mut self, previous: &[PlacedOutput]) -> Result<(), RemoteOutputError> {
        remote::restore_outputs(self.connection, self.runtime_dir, self.live, previous).await
    }

    /// A new paired session recording `requested`; the live set stays until
    /// commit.
    async fn establish_replacement(
        &mut self,
        requested: &[PlacedOutput],
        binding: &mut ReconfigureBinding,
    ) -> Result<(), RemoteOutputError> {
        match remote::establish(
            self.connection,
            self.runtime_dir,
            requested,
            self.live.cursor_mode,
        )
        .await
        {
            Ok(replacement) => {
                binding.replacement = Some(replacement);
                Ok(())
            }
            Err(error) => {
                let _ = remote::place(self.connection, &self.live.placements()).await;
                Err(error)
            }
        }
    }
}

impl OutputProvider for Reconfigure<'_> {
    type Plan = Vec<PlacedOutput>;
    type Prepared = Prepared;
    type Binding = ReconfigureBinding;
    type Evidence = Vec<VerifiedOutput>;
    type Error = ReconfigureFailure;

    fn capabilities(&self) -> OutputCapabilities {
        capabilities_for(self.mode)
    }

    fn demand(&self, plan: &Self::Plan) -> OutputDemand {
        OutputDemand {
            exact_modes: true,
            persistent_desktop: true,
            headless: true,
            ..OutputDemand::new(plan.len())
        }
    }

    fn preflight(
        &mut self,
        plan: &Self::Plan,
        _context: &OutputContext,
    ) -> Result<Self::Prepared, Self::Error> {
        let change = plan_reconfiguration(&self.live.placed, plan, self.mode)?;
        Ok(Prepared {
            change,
            requested: plan.clone(),
        })
    }

    async fn bind(
        &mut self,
        prepared: Self::Prepared,
    ) -> Result<Self::Binding, BindFailure<Self::Error>> {
        let previous = self.live.placed.clone();
        let mut binding = ReconfigureBinding {
            change: prepared.change.clone(),
            previous: previous.clone(),
            replacement: None,
            count_changed_in_place: false,
            evidence: Vec::new(),
            armed: true,
        };
        let outcome = match &prepared.change {
            Reconfiguration::Unchanged => Ok(()),
            Reconfiguration::InPlace { resized, .. } => {
                let result = remote::resize_in_place(
                    self.connection,
                    self.runtime_dir,
                    self.live,
                    &prepared.requested,
                    resized,
                )
                .await;
                if result.is_err() {
                    if let Err(rollback) = self.restore(&previous).await {
                        return Err(BindFailure {
                            source: result.err().map_or_else(
                                || ReconfigureFailure::Outputs(rollback),
                                ReconfigureFailure::Outputs,
                            ),
                            rollback: None,
                        });
                    }
                }
                result
            }
            Reconfiguration::Recreate => {
                // Another count changes on the running sessions: the kept
                // outputs keep their streams and input stays linked. A
                // replacement set is the fallback.
                let in_place = if prepared.requested.len() > previous.len() {
                    remote::add_outputs(
                        self.connection,
                        self.runtime_dir,
                        self.live,
                        &prepared.requested,
                    )
                    .await
                } else {
                    remote::remove_outputs(
                        self.connection,
                        self.runtime_dir,
                        self.live,
                        &prepared.requested,
                    )
                    .await
                };
                match in_place {
                    Ok(()) => {
                        binding.count_changed_in_place = true;
                        Ok(())
                    }
                    Err(error) if self.live.placed.len() == previous.len() => {
                        tracing::warn!(
                            target: target::SESSION,
                            %error,
                            "outputs could not be added on the running session; recording a replacement set"
                        );
                        if let Err(rollback) = self.restore(&previous).await {
                            return Err(BindFailure {
                                source: ReconfigureFailure::Outputs(rollback),
                                rollback: None,
                            });
                        }
                        self.establish_replacement(&prepared.requested, &mut binding)
                            .await
                    }
                    Err(error) => {
                        // Outputs were already stopped and kept ones may run
                        // a new size: restore every kept size and grow back
                        // to the previous count before reporting.
                        if let Err(rollback) = self.restore(&previous).await {
                            tracing::warn!(target: target::SESSION, %rollback, "the previous outputs could not be restored");
                        }
                        Err(error)
                    }
                }
            }
        };
        outcome.map_err(|error| BindFailure {
            source: ReconfigureFailure::Outputs(error),
            rollback: None,
        })?;
        Ok(binding)
    }

    async fn verify(&mut self, binding: &mut Self::Binding) -> Result<(), Self::Error> {
        let set = binding.replacement.as_ref().unwrap_or(&*self.live);
        binding.evidence = remote::verify(self.connection, set).await?;
        Ok(())
    }

    async fn commit(&mut self, binding: &mut Self::Binding) -> Result<(), Self::Error> {
        if let Some(mut replacement) = binding.replacement.take() {
            std::mem::swap(self.live, &mut replacement);
            // From the swap on the new set is live: the transaction is
            // committed whatever follows. `replacement` is now the previous
            // set; stopping its session removes its monitors, after which
            // Mutter re-lays out the rest. Placing again is best effort here
            // and the placement keeper repeats it on every monitor change.
            binding.armed = false;
            replacement.sessions.stop().await;
            if let Err(error) = remote::place(self.connection, &self.live.placements()).await {
                tracing::warn!(target: target::SESSION, %error, "committed outputs not placed yet; the placement keeper retries");
            }
        }
        binding.armed = false;
        Ok(())
    }

    async fn rollback(&mut self, binding: &mut Self::Binding) -> Result<(), Self::Error> {
        if !binding.armed {
            return Ok(());
        }
        binding.armed = false;
        if let Some(mut replacement) = binding.replacement.take() {
            replacement.sessions.stop().await;
            remote::place(self.connection, &self.live.placements()).await?;
            return Ok(());
        }
        if binding.count_changed_in_place {
            let previous = binding.previous.clone();
            self.restore(&previous).await?;
            return Ok(());
        }
        if matches!(binding.change, Reconfiguration::InPlace { .. }) {
            let previous = binding.previous.clone();
            self.restore(&previous).await?;
        }
        Ok(())
    }

    fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence {
        &binding.evidence
    }

    fn is_armed(&self, binding: &Self::Binding) -> bool {
        binding.armed
    }
}

/// The session-bound output socket and what reconfiguration needs.
pub struct OutputService {
    listener: UnixListener,
    uid: u32,
    connection: Connection,
    runtime_dir: PathBuf,
    mode: VirtualOutputMode,
    live: LiveOutputs,
    facts: Arc<SessionFacts>,
    input: Arc<InputService>,
    placement: Arc<watch::Sender<remote::DesiredPlacement>>,
    session_log_id: CorrelationId,
    snapshot: Arc<OutputSnapshot>,
    transaction: tokio::sync::Mutex<()>,
    colour: CompositorColour,
    /// How the session's streams report the cursor; wide streams too.
    cursor_mode: super::StreamCursorMode,
    /// The wide-capture streams of one output generation, while recorded.
    wide: tokio::sync::Mutex<Option<(u64, MonitorCasts)>>,
}

/// The live outputs as last committed, answered without waiting for a
/// running reconfiguration.
#[derive(Debug)]
pub struct OutputSnapshot(std::sync::RwLock<(u64, Vec<ReadyOutput>, Vec<ReadyStream>)>);

impl OutputSnapshot {
    /// The snapshot of `set` at `generation`.
    #[must_use]
    pub fn new(generation: u64, set: &RemoteSet) -> Self {
        Self(std::sync::RwLock::new((
            generation,
            set.ready_outputs(),
            set.ready_streams(),
        )))
    }

    /// Records `set` as live at `generation`.
    pub fn update(&self, generation: u64, set: &RemoteSet) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            (generation, set.ready_outputs(), set.ready_streams());
    }

    fn current(&self) -> (u64, Vec<ReadyOutput>, Vec<ReadyStream>) {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn reply(&self, error: Option<String>) -> OutputReply {
        let (generation, outputs, streams) = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        OutputReply {
            ok: error.is_none(),
            error,
            generation,
            outputs,
            streams,
        }
    }
}

/// What [`OutputService::bind`] needs.
pub struct OutputServiceParts {
    /// Session user.
    pub uid: u32,
    /// Private bus connection.
    pub connection: Connection,
    /// Private runtime directory.
    pub runtime_dir: PathBuf,
    /// Output mode.
    pub mode: VirtualOutputMode,
    /// Live outputs.
    pub live: LiveOutputs,
    /// Input facts to follow the outputs.
    pub facts: Arc<SessionFacts>,
    /// Input service, to release held input first.
    pub input: Arc<InputService>,
    /// The placement keeper's desired layout.
    pub placement: Arc<watch::Sender<remote::DesiredPlacement>>,
    /// Session log id.
    pub session_log_id: CorrelationId,
    /// What `hello` answers.
    pub snapshot: Arc<OutputSnapshot>,
    /// The colour state the session's compositor composes in.
    pub colour: CompositorColour,
    /// How the session's streams report the cursor.
    pub cursor_mode: super::StreamCursorMode,
}

impl OutputService {
    /// Binds the socket at `path`, readable and writable by its owner only.
    ///
    /// # Errors
    ///
    /// When the socket cannot be created.
    pub fn bind(path: &Path, parts: OutputServiceParts) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            uid: parts.uid,
            connection: parts.connection,
            runtime_dir: parts.runtime_dir,
            mode: parts.mode,
            live: parts.live,
            facts: parts.facts,
            input: parts.input,
            placement: parts.placement,
            session_log_id: parts.session_log_id,
            snapshot: parts.snapshot,
            transaction: tokio::sync::Mutex::new(()),
            colour: parts.colour,
            cursor_mode: parts.cursor_mode,
            wide: tokio::sync::Mutex::new(None),
        })
    }

    /// Serves up to [`MAX_CONNECTIONS`] connections at once, one command
    /// per line, until the future is dropped. `hello` answers from the
    /// snapshot, so it never waits behind a running reconfiguration; a
    /// second `configure` while one runs is refused; a connection idle for
    /// [`IDLE_TIMEOUT`] is closed.
    pub async fn serve(&self) {
        let mut connections = futures_util::stream::FuturesUnordered::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let peer = stream.peer_cred().ok().map(|cred| cred.uid());
                    if !matches!(peer, Some(uid) if uid == 0 || uid == self.uid) {
                        tracing::warn!(target: target::SESSION, peer_uid = ?peer, "refused an output connection from another user");
                        continue;
                    }
                    // The last slot is the Pier's (root): a session-user
                    // process holding connections open never locks it out.
                    let limit = if peer == Some(0) { MAX_CONNECTIONS } else { MAX_CONNECTIONS - 1 };
                    if connections.len() >= limit {
                        tracing::warn!(target: target::SESSION, "refused an output connection: too many open");
                        continue;
                    }
                    connections.push(self.serve_connection(stream));
                }
                Some(()) = futures_util::StreamExt::next(&mut connections), if !connections.is_empty() => {}
            }
        }
    }

    async fn serve_connection(&self, stream: tokio::net::UnixStream) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BoundedLineReader::new(BufReader::new(reader));
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout(IDLE_TIMEOUT, reader.next_bounded_line()).await
        {
            let reply = if line.truncated || line.text.len() > MAX_COMMAND_BYTES {
                self.snapshot.reply(Some("command too long".to_owned()))
            } else {
                match serde_json::from_str::<OutputCommand>(&line.text) {
                    Ok(OutputCommand::Hello) => self.snapshot.reply(None),
                    Ok(OutputCommand::Configure {
                        generation,
                        outputs,
                    }) => match self.transaction.try_lock() {
                        Ok(_running) => {
                            let error = self.configure(generation, &outputs).await.err();
                            self.snapshot.reply(error.map(|error| error.to_string()))
                        }
                        Err(_) => self.snapshot.reply(Some(
                            "another output reconfiguration is in progress".to_owned(),
                        )),
                    },
                    Ok(OutputCommand::WideStreams { generation }) => {
                        match self.transaction.try_lock() {
                            Ok(_running) => self.wide_streams(generation).await,
                            Err(_) => self
                                .snapshot
                                .reply(Some("an output reconfiguration is in progress".to_owned())),
                        }
                    }
                    Err(_) => self.snapshot.reply(Some("malformed command".to_owned())),
                }
            };
            let mut encoded = serde_json::to_vec(&reply).unwrap_or_default();
            encoded.push(b'\n');
            if !matches!(
                tokio::time::timeout(REPLY_TIMEOUT, writer.write_all(&encoded)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    }

    /// Records the current outputs again for wide capture, once per output
    /// generation.
    async fn wide_streams(&self, generation: u64) -> OutputReply {
        if self.colour != CompositorColour::ForcedHdr {
            return self.snapshot.reply(Some(
                "the session's compositor does not compose in a wide colour state".to_owned(),
            ));
        }
        let (current, outputs, streams) = self.snapshot.current();
        if generation != current {
            return self.snapshot.reply(Some(format!(
                "wide streams were asked for output generation {generation}, the session is at \
                 {current}"
            )));
        }
        let mut wide = self.wide.lock().await;
        if let Some((recorded, casts)) = wide.as_ref() {
            if *recorded == current {
                return OutputReply {
                    ok: true,
                    error: None,
                    generation: current,
                    outputs,
                    streams: casts.streams().to_vec(),
                };
            }
        }
        if let Some((_, mut previous)) = wide.take() {
            self.live.0.read().await.sessions.release_input_monitors();
            previous.stop().await;
        }
        match MonitorCasts::start(
            &self.connection,
            &streams,
            self.cursor_mode.screencast().mutter_value(),
        )
        .await
        {
            Ok(casts) => {
                // The wide epoch consumes these streams, not the virtual
                // ones absolute input addresses: input moves to unstarted
                // monitor streams of the same outputs, which Mutter maps
                // whether consumed or not.
                let connectors = streams
                    .iter()
                    .map(|stream| stream.connector.clone())
                    .collect::<Vec<_>>();
                if let Err(error) = self
                    .live
                    .0
                    .read()
                    .await
                    .sessions
                    .record_input_monitors(&connectors)
                    .await
                {
                    tracing::warn!(
                        target: target::SESSION,
                        sid = %self.session_log_id,
                        %error,
                        "absolute input could not follow the wide-capture streams; the pointer may not move"
                    );
                }
                let reply = OutputReply {
                    ok: true,
                    error: None,
                    generation: current,
                    outputs,
                    streams: casts.streams().to_vec(),
                };
                tracing::info!(
                    target: target::SESSION,
                    sid = %self.session_log_id,
                    generation = current,
                    nodes = ?reply.streams.iter().map(|stream| stream.node_id).collect::<Vec<_>>(),
                    "outputs recorded for wide capture"
                );
                *wide = Some((current, casts));
                reply
            }
            Err(error) => self
                .snapshot
                .reply(Some(format!("wide streams could not be recorded: {error}"))),
        }
    }

    /// Stops the wide-capture streams, if any.
    pub async fn stop_wide(&self) {
        if let Some((_, mut casts)) = self.wide.lock().await.take() {
            self.live.0.read().await.sessions.release_input_monitors();
            casts.stop().await;
        }
    }

    async fn configure(
        &self,
        generation: u64,
        outputs: &[OutputPlacement],
    ) -> Result<(), ReconfigureFailure> {
        let current = self.facts.snapshot().generation;
        check_next_generation(current, generation)?;
        // Wide streams record the outputs being replaced.
        self.stop_wide().await;
        let requested = outputs
            .iter()
            .map(OutputPlacement::placed)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                ReconfigureFailure::Transaction(
                    "an output size or refresh is outside what a compositor is asked for"
                        .to_owned(),
                )
            })?;
        tracing::info!(
            target: target::SESSION,
            sid = %self.session_log_id,
            generation,
            outputs = requested.len(),
            "output reconfiguration begin"
        );
        self.placement.send_replace(None);
        self.input.release_held(&self.live).await;
        let mut live = self.live.0.write().await;
        let outcome = {
            let transaction = OutputTransaction::acquire(
                Reconfigure {
                    connection: &self.connection,
                    runtime_dir: &self.runtime_dir,
                    mode: self.mode,
                    live: &mut live,
                },
                &requested,
                &OutputContext::new(self.session_log_id.clone()),
            )
            .await;
            match transaction {
                Ok(mut transaction) => transaction
                    .commit()
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            }
        };
        self.placement.send_replace(Some(live.placements()));
        if outcome.is_ok() {
            self.facts.advance(generation, live.ready_streams());
        }
        // The snapshot always states what is live now: the new outputs, or
        // the ones a refused or rolled-back transaction kept.
        self.snapshot
            .update(self.facts.snapshot().generation, &live);
        match outcome {
            Ok(()) => {
                tracing::info!(
                    target: target::SESSION,
                    sid = %self.session_log_id,
                    generation,
                    outputs = ?live
                        .ready_outputs()
                        .iter()
                        .map(|output| format!("{}={}x{}@{}", output.connector, output.width, output.height, output.refresh_hz))
                        .collect::<Vec<_>>(),
                    "output reconfiguration committed"
                );
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    target: target::SESSION,
                    sid = %self.session_log_id,
                    generation,
                    %error,
                    "output reconfiguration failed; previous outputs kept"
                );
                Err(ReconfigureFailure::Transaction(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn input_is_refused_at_once_while_outputs_are_reconfigured() {
        let live = RwLock::new(1_u64);
        assert!(unless_reconfiguring(&live).is_ok());
        let transaction = live.write().await;
        let started = std::time::Instant::now();
        assert!(matches!(
            unless_reconfiguring(&live),
            Err(MutterError::Reconfiguring)
        ));
        assert!(started.elapsed() < Duration::from_millis(50));
        drop(transaction);
        assert_eq!(*unless_reconfiguring(&live).unwrap(), 1);
    }
}
