//! The session user's half of `audio.local_playback`: a mute lease on a
//! PulseAudio-protocol server (PulseAudio itself or `pipewire-pulse`).
//!
//! Every decision — what to mute, what to move, what counts as evidence and
//! what to undo — is [`arcen_session::local_playback`]'s. This adapter reads
//! the server through `pactl` into a [`PlaybackGraph`], applies the actions
//! it is given, journals each one before applying it, and keeps the server
//! compliant for the whole desktop session with a `pactl subscribe` watcher.
//!
//! It runs as the session user, inside the session agent, so the lease lives
//! exactly as long as the desktop (detached time included), not as long as
//! one Deck attachment.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use arcen_session::local_playback::{
    enforce, evidence, restore, LeaseJournal, LocalPlaybackReport, MuteBasis, MuteEvidence,
    PlaybackGraph, RestoreAction, RoutingAction, Sink, Stream, StreamTarget,
    SESSION_SINK_DESCRIPTION,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::logging::target;

/// The lease journal, in the runtime directory of the server it mutates.
pub const JOURNAL_NAME: &str = "arcen-local-playback-lease.json";
/// The agent mode that restores a journal left by a session that ended
/// without releasing its lease.
pub const RECOVER_FLAG: &str = "--local-playback-recover";
/// The agent argument carrying the operator's policy.
pub const POLICY_FLAG: &str = "--local-playback";
/// The agent argument naming `pactl`.
pub const PACTL_FLAG: &str = "--pactl-bin";
/// `pactl` when the operator names none.
pub const DEFAULT_PACTL: &str = "/usr/bin/pactl";

const PACTL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PACTL_OUTPUT_BYTES: u64 = 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;
const MAX_ENFORCE_PASSES: usize = 4;
const EVENT_SETTLE: Duration = Duration::from_millis(50);
const RESUBSCRIBE_MIN: Duration = Duration::from_millis(250);
const RESUBSCRIBE_MAX: Duration = Duration::from_secs(4);
const ASOUND_CARDS: &str = "/proc/asound/cards";
/// How long a lease waits for the user's audio server to answer at all.
const SERVER_START_WAIT: Duration = Duration::from_millis(3000);
const SERVER_RETRY: Duration = Duration::from_millis(250);
#[cfg(target_os = "linux")]
const NO_FOLLOW: i32 = nix::libc::O_NOFOLLOW;
#[cfg(not(target_os = "linux"))]
const NO_FOLLOW: i32 = 0;

/// Why a lease operation failed.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("pactl {0} failed: {1}")]
    Pactl(String, String),
    #[error("pactl {0} timed out")]
    Timeout(String),
    #[error("pactl output was not understood: {0}")]
    Parse(&'static str),
    #[error("lease journal: {0}")]
    Journal(String),
    #[error("the session sink {0} does not exist on the audio server")]
    NoSessionSink(String),
}

/// One PulseAudio-protocol server, reached through `pactl`.
#[derive(Debug, Clone)]
pub struct PulseServer {
    pactl: PathBuf,
    /// `PULSE_SERVER` for every `pactl`; `None` inherits the process's
    /// own server lookup (`PULSE_RUNTIME_PATH`, then `XDG_RUNTIME_DIR`).
    address: Option<String>,
}

impl PulseServer {
    /// The server this process would reach by default.
    #[must_use]
    pub fn inherited(pactl: impl Into<PathBuf>) -> Self {
        Self {
            pactl: pactl.into(),
            address: None,
        }
    }

    /// The server listening on `socket`.
    #[must_use]
    pub fn at_socket(pactl: impl Into<PathBuf>, socket: &Path) -> Self {
        Self {
            pactl: pactl.into(),
            address: Some(format!("unix:{}", socket.display())),
        }
    }

    /// A short description for logs.
    #[must_use]
    pub fn describe(&self) -> &str {
        self.address.as_deref().unwrap_or("inherited")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.pactl);
        command
            .env("LC_ALL", "C")
            .env_remove("LANGUAGE")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(address) = &self.address {
            command.env("PULSE_SERVER", address);
        }
        tie_to_parent(&mut command);
        command
    }

    async fn run(&self, args: &[&str]) -> Result<String, LeaseError> {
        let label = args.first().copied().unwrap_or_default().to_owned();
        let mut command = self.command();
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| LeaseError::Pactl(label.clone(), error.to_string()))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let read = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            if let Some(stdout) = stdout {
                stdout
                    .take(MAX_PACTL_OUTPUT_BYTES + 1)
                    .read_to_end(&mut out)
                    .await
                    .map_err(|error| LeaseError::Pactl(label.clone(), error.to_string()))?;
            }
            if let Some(stderr) = stderr {
                let _ = stderr.take(4096).read_to_end(&mut err).await;
            }
            let status = child
                .wait()
                .await
                .map_err(|error| LeaseError::Pactl(label.clone(), error.to_string()))?;
            Ok::<_, LeaseError>((status, out, err))
        };
        let (status, out, err) = tokio::time::timeout(PACTL_TIMEOUT, read)
            .await
            .map_err(|_| LeaseError::Timeout(label.clone()))??;
        if out.len() as u64 > MAX_PACTL_OUTPUT_BYTES {
            return Err(LeaseError::Parse("output exceeded its bound"));
        }
        if !status.success() {
            let detail = String::from_utf8_lossy(&err).trim().to_owned();
            return Err(LeaseError::Pactl(label, detail));
        }
        String::from_utf8(out).map_err(|_| LeaseError::Parse("output is not UTF-8"))
    }

    /// `pactl subscribe`, as a command to spawn.
    pub(crate) fn subscribe_command(&self) -> Command {
        let mut command = self.command();
        command
            .arg("subscribe")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    /// `pactl info`.
    ///
    /// # Errors
    ///
    /// [`LeaseError`] when the server does not answer.
    pub async fn info(&self) -> Result<String, LeaseError> {
        self.run(&["info"]).await
    }

    /// Whether the server answers.
    pub async fn reachable(&self) -> bool {
        self.run(&["info"]).await.is_ok()
    }

    /// Whether the server answers within `wait`, asking again while it does
    /// not: a socket-activated server that the session's own login is
    /// starting (a first login can take seconds) is not an absent one.
    pub async fn reachable_within(&self, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if self.reachable().await {
                return true;
            }
            if tokio::time::Instant::now() + SERVER_RETRY >= deadline {
                return false;
            }
            tokio::time::sleep(SERVER_RETRY).await;
        }
    }

    /// The server's routing now. `move_refused` names streams the server
    /// already refused to move.
    pub async fn graph(&self, move_refused: &BTreeSet<u32>) -> Result<PlaybackGraph, LeaseError> {
        let info = self.run(&["info"]).await?;
        let sinks = self.run(&["list", "sinks"]).await?;
        let inputs = self.run(&["list", "sink-inputs"]).await?;
        let sinks = parse_sinks(&sinks);
        let names: HashMap<u32, String> = sinks
            .iter()
            .map(|(index, sink)| (*index, sink.name.clone()))
            .collect();
        let mut streams = parse_sink_inputs(&inputs, &names);
        for stream in &mut streams {
            stream.move_refused = move_refused.contains(&stream.id);
        }
        Ok(PlaybackGraph {
            default_sink: parse_default_sink(&info),
            sinks: sinks.into_iter().map(|(_, sink)| sink).collect(),
            streams,
        })
    }

    async fn apply(&self, action: &RoutingAction) -> Result<(), LeaseError> {
        match action {
            RoutingAction::MuteSink(name) => self.run(&["set-sink-mute", name, "1"]).await,
            RoutingAction::SetDefault(name) => self.run(&["set-default-sink", name]).await,
            RoutingAction::MoveStream { id, to } => {
                self.run(&["move-sink-input", &id.to_string(), to]).await
            }
        }
        .map(drop)
    }

    async fn undo(&self, action: &RestoreAction) -> Result<(), LeaseError> {
        let result = match action {
            RestoreAction::SetDefault(name) => self.run(&["set-default-sink", name]).await,
            RestoreAction::UnmuteSink(name) => self.run(&["set-sink-mute", name, "0"]).await,
            RestoreAction::UnloadModule(id) => self.run(&["unload-module", &id.to_string()]).await,
        };
        match result {
            // Gone between the reading and the undo: nothing left to undo.
            Err(LeaseError::Pactl(_, detail)) if already_gone(&detail) => Ok(()),
            other => other.map(drop),
        }
    }

    /// Loads a null sink called `name`, returning its module.
    pub async fn load_null_sink(&self, name: &str) -> Result<u32, LeaseError> {
        let description = SESSION_SINK_DESCRIPTION.replace(' ', "_");
        let output = self
            .run(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                &format!("sink_properties=device.description={description}"),
            ])
            .await?;
        output
            .trim()
            .parse()
            .map_err(|_| LeaseError::Parse("load-module returned no module id"))
    }

    /// Null-sink modules that create a sink called `name`.
    pub async fn null_sink_modules(&self, name: &str) -> Result<Vec<u32>, LeaseError> {
        let modules = self.run(&["list", "short", "modules"]).await?;
        Ok(parse_null_sink_modules(&modules, name))
    }

    /// Loads `module` with `args`, returning its id.
    pub async fn load_module(&self, module: &str, args: &[String]) -> Result<u32, LeaseError> {
        let mut command = vec!["load-module", module];
        command.extend(args.iter().map(String::as_str));
        self.run(&command)
            .await?
            .trim()
            .parse()
            .map_err(|_| LeaseError::Parse("load-module returned no module id"))
    }

    /// Unloads `module`.
    pub async fn unload_module(&self, module: u32) -> Result<(), LeaseError> {
        self.run(&["unload-module", &module.to_string()])
            .await
            .map(drop)
    }

    /// Makes `name` the default sink.
    pub async fn set_default_sink(&self, name: &str) -> Result<(), LeaseError> {
        self.run(&["set-default-sink", name]).await.map(drop)
    }
}

/// Ends a helper with this process, even when this process is killed
/// outright: a watcher's `pactl subscribe` must not outlive the lease.
pub(crate) fn tie_to_parent(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: runs in the child between fork and exec and only arms the
        // kernel parent-death signal (async-signal-safe prctl/getppid).
        unsafe {
            command
                .as_std_mut()
                .pre_exec(crate::session::identity::arm_parent_death_signal);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// Where the session sink comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSink {
    /// The sink exists already and outlives the lease (a private graph's
    /// configured sink).
    Existing(String),
    /// The lease creates a null sink of this name and removes it again.
    Create(String),
}

impl SessionSink {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Existing(name) | Self::Create(name) => name,
        }
    }
}

/// Counts of one enforcement pass, for the record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnforceCounts {
    pub muted_sinks: usize,
    pub moved_streams: usize,
    pub default_set: bool,
    pub failures: usize,
}

impl EnforceCounts {
    fn changed(&self) -> bool {
        self.muted_sinks + self.moved_streams > 0 || self.default_set
    }
}

/// An established local-playback lease. Release it with
/// [`PlaybackLease::release`]; dropping it leaves the journal for recovery.
#[derive(Debug)]
pub struct PlaybackLease {
    server: PulseServer,
    journal_path: PathBuf,
    journal: LeaseJournal,
    sink: SessionSink,
    basis: MuteBasis,
    move_refused: BTreeSet<u32>,
}

impl PlaybackLease {
    /// Starts a lease: creates the session sink if asked, makes it the
    /// default, mutes every other sink and moves every stream onto it.
    ///
    /// # Errors
    ///
    /// [`LeaseError`] when the server cannot be read, the journal cannot be
    /// written, or the session sink cannot be created. Whatever was already
    /// changed is in the journal.
    pub async fn establish(
        server: PulseServer,
        journal_path: PathBuf,
        sink: SessionSink,
        basis: MuteBasis,
    ) -> Result<Self, LeaseError> {
        let graph = server.graph(&BTreeSet::new()).await?;
        let prior_default = graph
            .default_sink
            .clone()
            .filter(|name| name != sink.name());
        let mut lease = Self {
            journal: LeaseJournal::new(sink.name(), prior_default),
            server,
            journal_path,
            sink,
            basis,
            move_refused: BTreeSet::new(),
        };
        lease.persist()?;
        lease.ensure_session_sink().await?;
        let counts = lease.enforce().await?;
        tracing::info!(
            target: target::AUDIO,
            event = "local_playback_lease",
            action = "established",
            server = lease.server.describe(),
            session_sink = lease.sink.name(),
            basis = lease.basis.token(),
            muted_sinks = counts.muted_sinks,
            moved_streams = counts.moved_streams,
            "local playback lease established"
        );
        Ok(lease)
    }

    /// A lease on `session_sink` that was never established, for tests of
    /// what runs a lease rather than of the lease itself.
    #[cfg(test)]
    pub(crate) fn detached(server: PulseServer, journal_path: PathBuf, session_sink: &str) -> Self {
        Self {
            server,
            journal_path,
            journal: LeaseJournal::new(session_sink, None),
            sink: SessionSink::Existing(session_sink.to_owned()),
            basis: MuteBasis::SessionSinkLease,
            move_refused: BTreeSet::new(),
        }
    }

    /// The session sink.
    #[must_use]
    pub fn session_sink(&self) -> &str {
        self.sink.name()
    }

    async fn ensure_session_sink(&mut self) -> Result<(), LeaseError> {
        let SessionSink::Create(name) = &self.sink else {
            let graph = self.server.graph(&self.move_refused).await?;
            if graph.sinks.iter().any(|sink| sink.name == self.sink.name()) {
                return Ok(());
            }
            return Err(LeaseError::NoSessionSink(self.sink.name().to_owned()));
        };
        let name = name.clone();
        let graph = self.server.graph(&self.move_refused).await?;
        if graph.sinks.iter().any(|sink| sink.name == name) {
            if self.journal.module_id.is_some() {
                return Ok(());
            }
            // A sink of this session's name that this lease did not load is
            // a leftover; replace it so the module that owns it is known.
            for module in self.server.null_sink_modules(&name).await? {
                let _ = self.server.unload_module(module).await;
            }
        }
        let module = self.server.load_null_sink(&name).await?;
        self.journal.module_id = Some(module);
        self.persist()
    }

    /// Brings the server back into compliance.
    ///
    /// # Errors
    ///
    /// [`LeaseError`] when the server cannot be read or the journal cannot
    /// be written. Individual actions that fail are counted, not fatal.
    pub async fn enforce(&mut self) -> Result<EnforceCounts, LeaseError> {
        let mut counts = EnforceCounts::default();
        for _ in 0..MAX_ENFORCE_PASSES {
            let graph = self.server.graph(&self.move_refused).await?;
            let actions = enforce(&graph, self.sink.name());
            if actions.is_empty() {
                break;
            }
            for action in &actions {
                let before = self.journal.clone();
                action.record(&mut self.journal);
                if self.journal != before {
                    self.persist()?;
                }
                match self.server.apply(action).await {
                    Ok(()) => match action {
                        RoutingAction::MuteSink(_) => counts.muted_sinks += 1,
                        RoutingAction::SetDefault(_) => counts.default_set = true,
                        RoutingAction::MoveStream { .. } => counts.moved_streams += 1,
                    },
                    Err(error) => {
                        counts.failures += 1;
                        if let RoutingAction::MoveStream { id, .. } = action {
                            self.move_refused.insert(*id);
                        }
                        tracing::debug!(target: target::AUDIO, ?action, %error, "local playback action failed");
                    }
                }
            }
        }
        Ok(counts)
    }

    /// What the server does now, as evidence for this lease.
    pub async fn report(&self) -> LocalPlaybackReport {
        let evidence = match self.settled_graph().await {
            Ok(graph) => evidence(&graph, self.sink.name(), true),
            Err(_) => MuteEvidence::observed(true, None),
        };
        LocalPlaybackReport {
            evidence,
            basis: self.basis,
        }
    }

    /// A reading of the server, taken again once when a sink came or went
    /// in the middle of it. A reading that is still unsettled is returned
    /// as it is, and its unknown streams refuse the evidence.
    async fn settled_graph(&self) -> Result<PlaybackGraph, LeaseError> {
        let graph = self.server.graph(&self.move_refused).await?;
        if graph.is_settled() {
            return Ok(graph);
        }
        tokio::time::sleep(EVENT_SETTLE).await;
        self.server.graph(&self.move_refused).await
    }

    /// Keeps the server compliant until the future is dropped: every sink,
    /// stream or server change re-runs [`Self::enforce`], and a server that
    /// goes away is followed when it returns (its session sink recreated).
    pub async fn watch(&mut self) {
        let mut retry = RESUBSCRIBE_MIN;
        loop {
            match self.watch_once().await {
                Ok(()) => retry = RESUBSCRIBE_MIN,
                Err(error) => {
                    tracing::warn!(
                        target: target::AUDIO,
                        event = "local_playback_lease",
                        action = "watch_interrupted",
                        %error,
                        "local playback watcher lost its audio server; retrying"
                    );
                }
            }
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(RESUBSCRIBE_MAX);
            if self.ensure_session_sink().await.is_ok() {
                let _ = self.enforce_logged("reconnected").await;
            }
        }
    }

    async fn watch_once(&mut self) -> Result<(), LeaseError> {
        let mut command = self.server.command();
        command
            .arg("subscribe")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| LeaseError::Pactl("subscribe".into(), error.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LeaseError::Pactl("subscribe".into(), "no stdout".into()))?;
        let mut lines = BufReader::new(stdout).lines();
        // A change that happened between establishing and subscribing.
        self.enforce_logged("subscribed").await?;
        loop {
            let Ok(Some(line)) = lines.next_line().await else {
                let _ = child.start_kill();
                return Err(LeaseError::Pactl(
                    "subscribe".into(),
                    "the server closed the subscription".into(),
                ));
            };
            if !event_needs_enforcement(&line) {
                continue;
            }
            // Coalesce a burst (our own changes produce events too).
            let settle = tokio::time::sleep(EVENT_SETTLE);
            tokio::pin!(settle);
            loop {
                tokio::select! {
                    () = &mut settle => break,
                    next = lines.next_line() => if !matches!(next, Ok(Some(_))) { break },
                }
            }
            self.enforce_logged("event").await?;
        }
    }

    async fn enforce_logged(&mut self, cause: &'static str) -> Result<(), LeaseError> {
        let counts = self.enforce().await?;
        if counts.changed() || counts.failures > 0 {
            tracing::info!(
                target: target::AUDIO,
                event = "local_playback_lease",
                action = "enforced",
                cause,
                muted_sinks = counts.muted_sinks,
                moved_streams = counts.moved_streams,
                default_set = counts.default_set,
                failures = counts.failures,
                "local playback lease re-applied"
            );
        }
        Ok(())
    }

    /// Ends the lease, undoing exactly what it changed.
    ///
    /// # Errors
    ///
    /// [`LeaseError`] when the server cannot be read or an undo fails; the
    /// journal then stays for recovery.
    pub async fn release(self) -> Result<(), LeaseError> {
        let result = undo_journal(&self.server, &self.journal).await;
        match &result {
            Ok(()) => {
                remove_journal(&self.journal_path)?;
                tracing::info!(
                    target: target::AUDIO,
                    event = "local_playback_lease",
                    action = "released",
                    session_sink = self.sink.name(),
                    "local playback lease released"
                );
            }
            Err(error) => tracing::warn!(
                target: target::AUDIO,
                event = "local_playback_lease",
                action = "release_failed",
                %error,
                journal = %self.journal_path.display(),
                "local playback lease could not be fully released; its journal is kept for recovery"
            ),
        }
        result
    }

    fn persist(&self) -> Result<(), LeaseError> {
        write_journal(&self.journal_path, &self.journal)
    }
}

async fn undo_journal(server: &PulseServer, journal: &LeaseJournal) -> Result<(), LeaseError> {
    let graph = server.graph(&BTreeSet::new()).await?;
    // Modules are only ever unloaded as the ones that create the session
    // sink now, never by a recorded id alone (see `restore`).
    let modules = server.null_sink_modules(&journal.session_sink).await?;
    let mut first_error = None;
    for action in restore(journal, &graph, &modules) {
        if let Err(error) = server.undo(&action).await {
            tracing::warn!(target: target::AUDIO, ?action, %error, "local playback restore step failed");
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Restores a journal a previous lease left behind.
///
/// Returns `Ok(false)` when there was nothing to recover.
///
/// # Errors
///
/// [`LeaseError`] when a journal exists and could not be restored; it is
/// kept for a later attempt.
pub async fn recover(server: &PulseServer, journal_path: &Path) -> Result<bool, LeaseError> {
    let Some(journal) = read_journal(journal_path)? else {
        return Ok(false);
    };
    if !journal.is_supported() {
        return Err(LeaseError::Journal(format!(
            "unsupported journal version {}",
            journal.version
        )));
    }
    undo_journal(server, &journal).await?;
    remove_journal(journal_path)?;
    tracing::info!(
        target: target::AUDIO,
        event = "local_playback_lease",
        action = "recovered",
        session_sink = %journal.session_sink,
        "a previous session's local playback lease was restored"
    );
    Ok(true)
}

/// A lease on the user's own server, or the evidence that none is needed.
#[derive(Debug)]
pub struct SharedLeaseOutcome {
    pub lease: Option<PlaybackLease>,
    pub report: LocalPlaybackReport,
}

/// Starts a lease on the user's own (shared) server after restoring any
/// lease a previous session left behind. Without a server, a host with no
/// sound device at all is silent by construction; anything else is
/// unproven.
pub async fn establish_shared_lease(
    server: PulseServer,
    journal: PathBuf,
    sink: Option<String>,
) -> SharedLeaseOutcome {
    let unproven = LocalPlaybackReport {
        evidence: MuteEvidence::observed(true, None),
        basis: MuteBasis::SessionSinkLease,
    };
    if !server.reachable_within(SERVER_START_WAIT).await {
        let report = if host_has_no_sound_card() {
            LocalPlaybackReport::no_local_output()
        } else {
            unproven
        };
        tracing::warn!(
            target: target::AUDIO,
            event = "local_playback_lease",
            action = "no_server",
            basis = report.basis.token(),
            honoured = report.evidence.honoured,
            "the user's audio server does not answer"
        );
        return SharedLeaseOutcome {
            lease: None,
            report,
        };
    }
    if let Err(error) = recover(&server, &journal).await {
        tracing::warn!(target: target::AUDIO, %error, "a previous local playback lease could not be restored");
    }
    let Some(sink) = sink else {
        return SharedLeaseOutcome {
            lease: None,
            report: unproven,
        };
    };
    match PlaybackLease::establish(
        server,
        journal,
        SessionSink::Create(sink),
        MuteBasis::SessionSinkLease,
    )
    .await
    {
        Ok(lease) => {
            let report = lease.report().await;
            SharedLeaseOutcome {
                lease: Some(lease),
                report,
            }
        }
        Err(error) => {
            tracing::warn!(target: target::AUDIO, %error, "local playback lease could not be established");
            SharedLeaseOutcome {
                lease: None,
                report: unproven,
            }
        }
    }
}

/// `pactl info` → the default source.
#[must_use]
pub fn parse_default_source(info: &str) -> Option<String> {
    info.lines()
        .find_map(|line| line.strip_prefix("Default Source: "))
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "(null)")
        .map(str::to_owned)
}

/// Whether `pactl`'s error says the object no longer exists.
fn already_gone(detail: &str) -> bool {
    detail.contains("No such entity")
}

/// Whether this host has no sound device at all (`/proc/asound/cards`
/// absent, or listing none).
#[must_use]
pub fn host_has_no_sound_card() -> bool {
    match std::fs::read_to_string(ASOUND_CARDS) {
        Ok(cards) => cards_list_none(&cards),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

fn cards_list_none(cards: &str) -> bool {
    !cards.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|first| first.chars().all(|c| c.is_ascii_digit()))
    })
}

fn write_journal(path: &Path, journal: &LeaseJournal) -> Result<(), LeaseError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let bytes =
        serde_json::to_vec(journal).map_err(|error| LeaseError::Journal(error.to_string()))?;
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(NO_FOLLOW)
        .open(&temporary)
        .map_err(|error| LeaseError::Journal(format!("create: {error}")))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| LeaseError::Journal(format!("write: {error}")))?;
    std::fs::rename(&temporary, path).map_err(|error| LeaseError::Journal(format!("arm: {error}")))
}

fn read_journal(path: &Path) -> Result<Option<LeaseJournal>, LeaseError> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(NO_FOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(LeaseError::Journal(format!("open: {error}"))),
    };
    let mut bytes = Vec::new();
    file.take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| LeaseError::Journal(format!("read: {error}")))?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(LeaseError::Journal("journal exceeds its bound".into()));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| LeaseError::Journal(format!("parse: {error}")))
}

fn remove_journal(path: &Path) -> Result<(), LeaseError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LeaseError::Journal(format!("remove: {error}"))),
    }
}

/// `pactl list sinks` (`LC_ALL=C`) → index and sink.
#[must_use]
pub fn parse_sinks(text: &str) -> Vec<(u32, Sink)> {
    parse_blocks(text, "Sink #")
        .into_iter()
        .filter_map(|(index, fields)| {
            Some((
                index,
                Sink {
                    name: fields.get("Name")?.clone(),
                    muted: fields.get("Mute").is_some_and(|mute| mute == "yes"),
                },
            ))
        })
        .collect()
}

/// `pactl list sink-inputs` (`LC_ALL=C`) → streams, with sink indices
/// resolved through `sink_names`.
#[must_use]
pub fn parse_sink_inputs(text: &str, sink_names: &HashMap<u32, String>) -> Vec<Stream> {
    parse_blocks(text, "Sink Input #")
        .into_iter()
        .map(|(id, fields)| Stream {
            id,
            // A sink index this reading cannot name (a sink that appeared or
            // went between the two reads) is unknown, never "nowhere".
            target: match fields.get("Sink") {
                None => StreamTarget::Unconnected,
                Some(index) => index
                    .parse::<u32>()
                    .ok()
                    .and_then(|index| sink_names.get(&index).cloned())
                    .map_or(StreamTarget::Unknown, StreamTarget::Sink),
            },
            muted: fields.get("Mute").is_some_and(|mute| mute == "yes"),
            move_refused: false,
        })
        .collect()
}

/// `pactl info` → the default sink.
#[must_use]
pub fn parse_default_sink(info: &str) -> Option<String> {
    info.lines()
        .find_map(|line| line.strip_prefix("Default Sink: "))
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "(null)")
        .map(str::to_owned)
}

/// `pactl list short modules` → the `module-null-sink` modules whose
/// arguments create `sink_name`.
#[must_use]
pub fn parse_null_sink_modules(text: &str, sink_name: &str) -> Vec<u32> {
    let wanted = format!("sink_name={sink_name}");
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.trim().parse::<u32>().ok()?;
            (fields.next()? == "module-null-sink"
                && fields
                    .next()
                    .is_some_and(|args| args.split_whitespace().any(|arg| arg == wanted)))
            .then_some(id)
        })
        .collect()
}

/// Whether a `pactl subscribe` line can change what the lease enforces.
#[must_use]
pub fn event_needs_enforcement(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("Event '") else {
        return false;
    };
    let Some((kind, rest)) = rest.split_once("' on ") else {
        return false;
    };
    let facility = rest.split(" #").next().unwrap_or(rest).trim();
    matches!(kind, "new" | "change" | "remove")
        && matches!(facility, "sink" | "sink-input" | "server")
}

fn parse_blocks(text: &str, header: &str) -> Vec<(u32, HashMap<String, String>)> {
    let mut blocks = Vec::new();
    let mut current: Option<(u32, HashMap<String, String>)> = None;
    for line in text.lines() {
        if let Some(index) = line.strip_prefix(header) {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = index
                .trim()
                .parse()
                .ok()
                .map(|index| (index, HashMap::new()));
            continue;
        }
        // Top-level fields are indented by exactly one tab; properties and
        // continuation lines by more.
        let Some(field) = line.strip_prefix('\t') else {
            continue;
        };
        if field.starts_with('\t') || field.starts_with(' ') {
            continue;
        }
        if let (Some((_, fields)), Some((key, value))) = (current.as_mut(), field.split_once(':')) {
            fields
                .entry(key.trim().to_owned())
                .or_insert_with(|| value.trim().to_owned());
        }
    }
    if let Some(block) = current {
        blocks.push(block);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    const PW_SINKS: &str = include_str!("../../tests/fixtures/pactl/pipewire-pulse-1.4/sinks.txt");
    const PW_INPUTS: &str =
        include_str!("../../tests/fixtures/pactl/pipewire-pulse-1.4/sink-inputs.txt");
    const PW_INFO: &str = include_str!("../../tests/fixtures/pactl/pipewire-pulse-1.4/info.txt");
    const PW_MODULES: &str =
        include_str!("../../tests/fixtures/pactl/pipewire-pulse-1.4/modules.txt");
    const PW_EVENTS: &str =
        include_str!("../../tests/fixtures/pactl/pipewire-pulse-1.4/subscribe.txt");
    const PA_SINKS: &str = include_str!("../../tests/fixtures/pactl/pulseaudio-15/sinks.txt");
    const PA_INPUTS: &str =
        include_str!("../../tests/fixtures/pactl/pulseaudio-15/sink-inputs.txt");
    const PA_INFO: &str = include_str!("../../tests/fixtures/pactl/pulseaudio-15/info.txt");
    const PA_MODULES: &str = include_str!("../../tests/fixtures/pactl/pulseaudio-15/modules.txt");

    #[test]
    fn pipewire_pulse_text_parses_into_a_graph() {
        let sinks = parse_sinks(PW_SINKS);
        assert_eq!(
            sinks,
            vec![
                (
                    31,
                    Sink {
                        name: "arcen_session_sink".into(),
                        muted: false
                    }
                ),
                (
                    49,
                    Sink {
                        name: "lab_speaker".into(),
                        muted: false
                    }
                ),
            ]
        );
        let names = sinks
            .iter()
            .map(|(index, sink)| (*index, sink.name.clone()))
            .collect();
        let streams = parse_sink_inputs(PW_INPUTS, &names);
        assert_eq!(streams.len(), 3, "two paplay and one native pw-play");
        assert!(streams.iter().all(|stream| stream.target
            == StreamTarget::Sink("lab_speaker".into())
            && !stream.muted));
        assert_eq!(parse_default_sink(PW_INFO).as_deref(), Some("lab_speaker"));
        assert_eq!(
            parse_null_sink_modules(PW_MODULES, "lab_speaker"),
            vec![536_870_916]
        );
        assert!(parse_null_sink_modules(PW_MODULES, "lab").is_empty());
    }

    #[test]
    fn pulseaudio_15_text_parses_into_a_graph() {
        let sinks = parse_sinks(PA_SINKS);
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].0, 1);
        assert_eq!(sinks[0].1.name, "lab_speaker");
        let names = sinks
            .iter()
            .map(|(index, sink)| (*index, sink.name.clone()))
            .collect();
        let streams = parse_sink_inputs(PA_INPUTS, &names);
        assert_eq!(
            streams.iter().map(|stream| stream.id).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(parse_default_sink(PA_INFO).as_deref(), Some("lab_speaker"));
        assert_eq!(parse_null_sink_modules(PA_MODULES, "lab_speaker"), vec![19]);
    }

    #[test]
    fn a_muted_sink_and_stream_are_read_as_muted() {
        let sinks = parse_sinks("Sink #3\n\tName: a\n\tMute: yes\n\tProperties:\n\t\tMute: no\n");
        assert!(sinks[0].1.muted);
        let names = HashMap::from([(3, "a".to_owned())]);
        let streams = parse_sink_inputs("Sink Input #9\n\tSink: 3\n\tMute: yes\n", &names);
        assert!(streams[0].muted);
        assert_eq!(streams[0].target, StreamTarget::Sink("a".into()));
        let unknown = parse_sink_inputs("Sink Input #9\n\tSink: 4\n\tMute: no\n", &names);
        assert_eq!(
            unknown[0].target,
            StreamTarget::Unknown,
            "an index the reading cannot name is unknown, not unconnected"
        );
        let unconnected = parse_sink_inputs("Sink Input #9\n\tMute: no\n", &names);
        assert_eq!(unconnected[0].target, StreamTarget::Unconnected);
        assert!(already_gone("Failure: No such entity"));
        assert!(!already_gone("Failure: Access denied"));
    }

    #[test]
    fn only_sink_stream_and_server_events_wake_the_watcher() {
        assert!(!PW_EVENTS.lines().take(10).any(event_needs_enforcement));
        assert!(PW_EVENTS.lines().any(event_needs_enforcement));
        assert!(event_needs_enforcement("Event 'new' on sink-input #45"));
        assert!(event_needs_enforcement("Event 'change' on server"));
        assert!(event_needs_enforcement("Event 'remove' on sink #31"));
        assert!(!event_needs_enforcement("Event 'new' on source-output #4"));
        assert!(!event_needs_enforcement("Event 'change' on card #2"));
        assert!(!event_needs_enforcement("Got SIGINT, exiting."));
    }

    #[test]
    fn default_source_is_read_from_info() {
        assert_eq!(
            parse_default_source(PA_INFO).as_deref(),
            Some("lab_speaker.monitor")
        );
        assert_eq!(parse_default_source("Default Source: (null)\n"), None);
    }

    #[test]
    fn default_sink_absent_or_null_is_none() {
        assert_eq!(parse_default_sink("Server Name: x\n"), None);
        assert_eq!(parse_default_sink("Default Sink: (null)\n"), None);
    }

    #[test]
    fn sound_card_listing() {
        assert!(cards_list_none("--- no soundcards ---\n"));
        assert!(cards_list_none(""));
        assert!(!cards_list_none(
            " 0 [Dummy          ]: Dummy - Dummy\n                      Dummy 1\n"
        ));
    }

    #[test]
    fn journal_round_trips_through_the_file() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/arcen-test-artifacts")
            .join(format!("local-playback-journal-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(JOURNAL_NAME);
        assert!(read_journal(&path).unwrap().is_none());
        let mut journal = LeaseJournal::new("arcen_session_ab", Some("speaker".into()));
        journal.muted_sinks.insert("speaker".into());
        write_journal(&path, &journal).unwrap();
        assert_eq!(read_journal(&path).unwrap(), Some(journal));
        remove_journal(&path).unwrap();
        remove_journal(&path).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
