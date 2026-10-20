//! Silencing the host's own speakers for a session, and proving it.
//!
//! [`crate::pier_config::LocalPlayback`] is the operator's policy. This
//! module owns everything about that policy that is not an operating-system
//! call: what counts as evidence that the host is silent, when a session
//! must be refused for lack of it, and — for hosts whose audio server routes
//! streams between sinks — which routing changes make the host silent and
//! which of them must be undone afterwards.
//!
//! The routing model is deliberately generic (sinks, streams, a default
//! sink). A platform adapter reads its audio server into a
//! [`PlaybackGraph`], applies the [`RoutingAction`]s [`enforce`] returns,
//! records each one in a [`LeaseJournal`] *before* applying it, and at the
//! end applies the [`RestoreAction`]s [`restore`] returns. Only what the
//! lease changed is restored: a sink the user had muted stays muted.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::pier_config::LocalPlayback;

/// What a mute lease did, for the record.
///
/// Three separate facts, because collapsing them produces a field that reads
/// as "mute was applied" while reporting that it deliberately was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuteEvidence {
    /// Whether the host configuration asked for local playback to be silenced.
    pub requested: bool,
    /// What the host's audio system reports it is actually doing.
    ///
    /// `None` when it could not be read, which is not the same as audible and
    /// must not be treated as either answer.
    pub observed_muted: Option<bool>,
    /// Whether the observed state matches what was requested.
    ///
    /// This is the field a session decides on. A requested mute that is not
    /// honoured must refuse the session.
    pub honoured: bool,
}

impl MuteEvidence {
    /// Evidence from an observation: honoured when it matches the request.
    #[must_use]
    pub const fn observed(requested: bool, observed_muted: Option<bool>) -> Self {
        let honoured = match observed_muted {
            Some(muted) => muted == requested,
            None => false,
        };
        Self {
            requested,
            observed_muted,
            honoured,
        }
    }

    /// The operator asked for audible playback, and nothing was changed.
    #[must_use]
    pub const fn not_requested() -> Self {
        Self {
            requested: false,
            observed_muted: None,
            honoured: true,
        }
    }
}

/// Why a host is silent, alongside the evidence that it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuteBasis {
    /// The operator chose audible playback; nothing was muted.
    NotRequested,
    /// A session sink is the default and every other sink is muted.
    SessionSinkLease,
    /// The session's audio graph is private and holds no output device.
    PrivateGraph,
    /// There is no audio server and no sound device at all.
    NoLocalOutput,
    /// The platform silences output natively (a process tap).
    PlatformTap,
}

impl MuteBasis {
    /// The log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::SessionSinkLease => "session_sink_lease",
            Self::PrivateGraph => "private_graph",
            Self::NoLocalOutput => "no_local_output",
            Self::PlatformTap => "platform_tap",
        }
    }
}

/// Mute evidence with its basis, as one host reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPlaybackReport {
    pub evidence: MuteEvidence,
    pub basis: MuteBasis,
}

impl LocalPlaybackReport {
    /// The report for an operator who chose audible playback.
    #[must_use]
    pub const fn not_requested() -> Self {
        Self {
            evidence: MuteEvidence::not_requested(),
            basis: MuteBasis::NotRequested,
        }
    }

    /// The report for a host with neither an audio server nor a sound
    /// device: nothing can be heard, so the requested silence holds.
    #[must_use]
    pub const fn no_local_output() -> Self {
        Self {
            evidence: MuteEvidence {
                requested: true,
                observed_muted: Some(true),
                honoured: true,
            },
            basis: MuteBasis::NoLocalOutput,
        }
    }
}

/// Why a session was refused under the local-playback policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPlaybackRefusal {
    /// A mute was requested and the host observed something else.
    NotHonoured {
        requested: bool,
        observed_muted: Option<bool>,
    },
    /// A mute was requested and the host produced no evidence at all.
    NoEvidence,
}

impl Display for LocalPlaybackRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotHonoured {
                requested,
                observed_muted,
            } => write!(
                formatter,
                "local playback mute was not honoured (requested={requested}, observed_muted={observed_muted:?})"
            ),
            Self::NoEvidence => formatter.write_str("local playback mute produced no evidence"),
        }
    }
}

impl std::error::Error for LocalPlaybackRefusal {}

/// What the Deck is told when the host refused a session because it could
/// not silence its own speakers.
pub const DECK_REASON_NOT_MUTED: &str =
    "The host could not mute its own speakers, so it refused the session. See its agent log.";

/// Decides whether a session may be served under `policy`.
///
/// # Errors
///
/// [`LocalPlaybackRefusal`] when a mute is required and `evidence` does not
/// prove it.
pub fn admit(
    policy: LocalPlayback,
    evidence: Option<MuteEvidence>,
) -> Result<(), LocalPlaybackRefusal> {
    if !policy.requires_mute() {
        return Ok(());
    }
    match evidence {
        Some(evidence) if evidence.honoured && evidence.requested => Ok(()),
        Some(evidence) => Err(LocalPlaybackRefusal::NotHonoured {
            requested: evidence.requested,
            observed_muted: evidence.observed_muted,
        }),
        None => Err(LocalPlaybackRefusal::NoEvidence),
    }
}

/// One sink of an audio server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sink {
    pub name: String,
    pub muted: bool,
}

/// Where a playback stream plays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamTarget {
    /// Not connected to any sink: it plays nowhere.
    Unconnected,
    /// Playing to this sink.
    Sink(String),
    /// Connected to a sink the reading could not name (one that appeared or
    /// went between two reads of the server). Never evidence of silence.
    Unknown,
}

/// One playback stream of an audio server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    pub id: u32,
    pub target: StreamTarget,
    pub muted: bool,
    /// The server refused to move it (a stream that pins its sink).
    pub move_refused: bool,
}

/// An audio server's routing at one instant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackGraph {
    pub default_sink: Option<String>,
    pub sinks: Vec<Sink>,
    pub streams: Vec<Stream>,
}

impl PlaybackGraph {
    fn sink(&self, name: &str) -> Option<&Sink> {
        self.sinks.iter().find(|sink| sink.name == name)
    }

    /// Whether every stream's sink is known (a reading taken while a sink
    /// came or went is worth taking again before it is judged).
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.streams.iter().all(|stream| match &stream.target {
            StreamTarget::Unconnected => true,
            StreamTarget::Sink(name) => self.sink(name).is_some(),
            StreamTarget::Unknown => false,
        })
    }
}

/// A change that makes the host silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingAction {
    MuteSink(String),
    SetDefault(String),
    MoveStream { id: u32, to: String },
}

impl RoutingAction {
    /// Records this action in `journal` so that it can be undone. Call it
    /// before applying the action: an action that was applied but never
    /// recorded can never be restored.
    pub fn record(&self, journal: &mut LeaseJournal) {
        match self {
            Self::MuteSink(name) => {
                journal.muted_sinks.insert(name.clone());
            }
            Self::SetDefault(_) | Self::MoveStream { .. } => {}
        }
    }
}

/// The changes that silence every sink except `session_sink` and route every
/// stream to it, in the order they must be applied.
///
/// Other sinks are muted first, so a stream the server routes to an old
/// device between two passes is already silent. A stream the server will
/// not move stays where it is, silent on its muted sink: streams are never
/// muted themselves, because servers remember a stream's mute by
/// application and would mute that application's later streams too. The result is empty for a
/// graph that already complies, and for a graph without the session sink
/// (whose owner must create it first; [`evidence`] then refuses).
#[must_use]
pub fn enforce(graph: &PlaybackGraph, session_sink: &str) -> Vec<RoutingAction> {
    if graph.sink(session_sink).is_none() {
        return Vec::new();
    }
    let mut actions = Vec::new();
    for sink in &graph.sinks {
        if sink.name != session_sink && !sink.muted {
            actions.push(RoutingAction::MuteSink(sink.name.clone()));
        }
    }
    if graph.default_sink.as_deref() != Some(session_sink) {
        actions.push(RoutingAction::SetDefault(session_sink.to_owned()));
    }
    for stream in &graph.streams {
        let elsewhere = match &stream.target {
            StreamTarget::Unconnected => false,
            StreamTarget::Sink(sink) => sink != session_sink,
            StreamTarget::Unknown => true,
        };
        if elsewhere && !stream.move_refused {
            actions.push(RoutingAction::MoveStream {
                id: stream.id,
                to: session_sink.to_owned(),
            });
        }
    }
    actions
}

/// Whether `graph` is silent everywhere except `session_sink`, as evidence
/// for a request of `requested`.
#[must_use]
pub fn evidence(graph: &PlaybackGraph, session_sink: &str, requested: bool) -> MuteEvidence {
    let Some(_) = graph.sink(session_sink) else {
        return MuteEvidence::observed(requested, None);
    };
    let compliant = graph.default_sink.as_deref() == Some(session_sink)
        && graph
            .sinks
            .iter()
            .all(|sink| sink.name == session_sink || sink.muted)
        && graph.streams.iter().all(|stream| {
            stream.muted
                || match &stream.target {
                    StreamTarget::Unconnected => true,
                    StreamTarget::Sink(sink) => {
                        sink == session_sink || graph.sink(sink).is_some_and(|sink| sink.muted)
                    }
                    StreamTarget::Unknown => false,
                }
        });
    MuteEvidence::observed(requested, Some(compliant))
}

/// Current journal format.
pub const LEASE_JOURNAL_VERSION: u32 = 1;

/// What a mute lease changed, persisted before each change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseJournal {
    pub version: u32,
    /// The sink the lease routes to.
    pub session_sink: String,
    /// The server module that created the session sink, when the lease
    /// created it (and so must remove it).
    pub module_id: Option<u32>,
    /// The default sink before the lease.
    pub prior_default: Option<String>,
    /// Sinks the lease muted. Only these are unmuted.
    pub muted_sinks: BTreeSet<String>,
}

impl LeaseJournal {
    /// A journal for a lease about to start.
    #[must_use]
    pub fn new(session_sink: impl Into<String>, prior_default: Option<String>) -> Self {
        Self {
            version: LEASE_JOURNAL_VERSION,
            session_sink: session_sink.into(),
            module_id: None,
            prior_default,
            muted_sinks: BTreeSet::new(),
        }
    }

    /// Whether this journal is one this code can restore.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        self.version == LEASE_JOURNAL_VERSION
    }
}

/// A change that undoes part of a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreAction {
    SetDefault(String),
    UnmuteSink(String),
    UnloadModule(u32),
}

/// The changes that undo `journal` on `graph`, in the order they must be
/// applied: the previous default first, so that streams on the session sink
/// fall back to it when the session sink goes, and the module last.
///
/// Only sinks the lease muted, and that are still muted, are unmuted. The
/// previous default is restored only while it exists and the session sink
/// is still the default (otherwise someone chose another default since, and
/// that choice stands).
///
/// A module is unloaded only while the session sink still exists, and only
/// one of `session_sink_modules` (the server's modules that create a sink of
/// that name): the journal's module when it is among them, all of them
/// otherwise (a module whose id was never recorded). A recorded id is never
/// unloaded on its own word: after the server restarted it can name an
/// unrelated module.
#[must_use]
pub fn restore(
    journal: &LeaseJournal,
    graph: &PlaybackGraph,
    session_sink_modules: &[u32],
) -> Vec<RestoreAction> {
    let mut actions = Vec::new();
    if let Some(prior) = journal.prior_default.as_deref() {
        let default_is_ours = graph.default_sink.as_deref() == Some(journal.session_sink.as_str());
        if prior != journal.session_sink && default_is_ours && graph.sink(prior).is_some() {
            actions.push(RestoreAction::SetDefault(prior.to_owned()));
        }
    }
    for name in &journal.muted_sinks {
        if graph.sink(name).is_some_and(|sink| sink.muted) {
            actions.push(RestoreAction::UnmuteSink(name.clone()));
        }
    }
    if graph.sink(&journal.session_sink).is_some() {
        match journal.module_id {
            Some(module) if session_sink_modules.contains(&module) => {
                actions.push(RestoreAction::UnloadModule(module));
            }
            _ => actions.extend(
                session_sink_modules
                    .iter()
                    .copied()
                    .map(RestoreAction::UnloadModule),
            ),
        }
    }
    actions
}

/// Maximum length of a session tag in a sink name.
pub const MAX_SINK_TAG_BYTES: usize = 32;

/// The name of the session sink a lease creates in a shared (per-user)
/// audio server, unique per session so that a crashed session's sink is
/// never mistaken for the current one.
///
/// Returns `None` for a tag that is empty, too long, or not lowercase
/// ASCII alphanumerics.
#[must_use]
pub fn shared_server_session_sink(tag: &str) -> Option<String> {
    let valid = !tag.is_empty()
        && tag.len() <= MAX_SINK_TAG_BYTES
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    valid.then(|| format!("arcen_session_{tag}"))
}

/// The session sink of a private session audio graph.
pub const PRIVATE_SESSION_SINK: &str = "arcen_session_sink";
/// What the session sink is called where people see it.
pub const SESSION_SINK_DESCRIPTION: &str = "Arcen session audio";

/// How a session's audio is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAudioMode {
    /// The session uses the user's own audio server (the Xorg path).
    UserServer,
    /// The session's private graph carries all audio: a session sink, a
    /// private `PulseAudio`-protocol server, and no output device.
    PrivateGraph,
    /// The session's private graph tunnels to the user's `PulseAudio`, which
    /// holds the session sink.
    BridgedToUserPulse,
    /// Only `PulseAudio` clients are served; native `PipeWire` clients reach
    /// a graph with no sink.
    UserPulseOnly,
}

impl SessionAudioMode {
    /// The log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::UserServer => "user_server",
            Self::PrivateGraph => "private_graph",
            Self::BridgedToUserPulse => "bridged_to_user_pulse",
            Self::UserPulseOnly => "user_pulse_only",
        }
    }

    /// Whether native `PipeWire` applications are heard in this mode.
    #[must_use]
    pub const fn serves_native_pipewire(self) -> bool {
        !matches!(self, Self::UserPulseOnly)
    }

    /// Whether the session sink lives in the user's shared audio server, so
    /// a local-playback lease must mutate that server.
    #[must_use]
    pub const fn uses_shared_server(self) -> bool {
        !matches!(self, Self::PrivateGraph)
    }
}

/// What a host offers for a private Wayland session's audio, as found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WaylandAudioFacts {
    /// A `PulseAudio`-protocol server can run on the private `PipeWire`
    /// (`pipewire-pulse`'s configuration and module are installed).
    pub private_pulse_server: bool,
    /// `PipeWire`'s pulse-tunnel module is installed.
    pub pulse_tunnel_module: bool,
}

/// Why a Wayland session's audio is less than a private graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAudioDegradation {
    /// No `PulseAudio`-protocol server for the private graph: native and
    /// `PulseAudio` clients are bridged into the user's `PulseAudio`.
    NoPrivatePulseServer,
    /// Neither a private `PulseAudio`-protocol server nor a tunnel: native
    /// `PipeWire` clients are not heard.
    NativePipeWireUnserved,
}

impl SessionAudioDegradation {
    /// The log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NoPrivatePulseServer => "no_private_pulse_server",
            Self::NativePipeWireUnserved => "native_pipewire_unserved",
        }
    }
}

/// Chooses how a private Wayland session's audio is served.
#[must_use]
pub const fn resolve_wayland_audio(
    facts: WaylandAudioFacts,
) -> (SessionAudioMode, Option<SessionAudioDegradation>) {
    if facts.private_pulse_server {
        (SessionAudioMode::PrivateGraph, None)
    } else if facts.pulse_tunnel_module {
        (
            SessionAudioMode::BridgedToUserPulse,
            Some(SessionAudioDegradation::NoPrivatePulseServer),
        )
    } else {
        (
            SessionAudioMode::UserPulseOnly,
            Some(SessionAudioDegradation::NativePipeWireUnserved),
        )
    }
}

/// End-to-end latency the bridge tunnel targets, in milliseconds. `PipeWire`
/// defaults to 200 ms; 40 ms keeps two 20 ms capture packets in flight.
pub const BRIDGE_TUNNEL_LATENCY_MS: u32 = 40;
/// How often a broken bridge tunnel reconnects, where it can.
pub const BRIDGE_TUNNEL_RECONNECT_MS: u32 = 1000;

/// Whether a `PipeWire` of this version reconnects a broken pulse tunnel
/// (`reconnect.interval.ms`, 1.1.0 and later).
#[must_use]
pub const fn pulse_tunnel_reconnects(major: u32, minor: u32) -> bool {
    major > 1 || (major == 1 && minor >= 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(name: &str, muted: bool) -> Sink {
        Sink {
            name: name.to_owned(),
            muted,
        }
    }

    fn stream(id: u32, sink: Option<&str>, muted: bool, move_refused: bool) -> Stream {
        Stream {
            id,
            target: sink.map_or(StreamTarget::Unconnected, |sink| {
                StreamTarget::Sink(sink.to_owned())
            }),
            muted,
            move_refused,
        }
    }

    const SESSION: &str = "arcen_session_ab12";

    fn speaker_graph() -> PlaybackGraph {
        PlaybackGraph {
            default_sink: Some("speaker".into()),
            sinks: vec![
                sink("speaker", false),
                sink("headset", true),
                sink(SESSION, false),
            ],
            streams: vec![
                stream(7, Some("speaker"), false, false),
                stream(8, Some(SESSION), false, false),
                stream(9, None, false, false),
            ],
        }
    }

    #[test]
    fn admit_matrix() {
        let honoured = MuteEvidence::observed(true, Some(true));
        let refused = MuteEvidence::observed(true, Some(false));
        let unknown = MuteEvidence::observed(true, None);
        assert!(admit(LocalPlayback::Audible, None).is_ok());
        assert!(admit(LocalPlayback::Audible, Some(refused)).is_ok());
        assert!(admit(LocalPlayback::Muted, Some(honoured)).is_ok());
        assert_eq!(
            admit(LocalPlayback::Muted, None),
            Err(LocalPlaybackRefusal::NoEvidence)
        );
        assert_eq!(
            admit(LocalPlayback::Muted, Some(refused)),
            Err(LocalPlaybackRefusal::NotHonoured {
                requested: true,
                observed_muted: Some(false)
            })
        );
        let error = admit(LocalPlayback::Muted, Some(unknown)).unwrap_err();
        assert!(error.to_string().contains("mute was not honoured"));
        assert!(admit(LocalPlayback::Muted, Some(MuteEvidence::not_requested())).is_err());
        assert!(
            admit(
                LocalPlayback::Muted,
                Some(LocalPlaybackReport::no_local_output().evidence)
            )
            .is_ok()
        );
    }

    #[test]
    fn enforce_mutes_other_sinks_before_routing_and_moves_streams() {
        let actions = enforce(&speaker_graph(), SESSION);
        assert_eq!(
            actions,
            vec![
                RoutingAction::MuteSink("speaker".into()),
                RoutingAction::SetDefault(SESSION.into()),
                RoutingAction::MoveStream {
                    id: 7,
                    to: SESSION.into()
                },
            ]
        );
    }

    #[test]
    fn a_stream_that_will_not_move_stays_silent_on_its_muted_sink() {
        let mut graph = speaker_graph();
        graph.streams[0].move_refused = true;
        let actions = enforce(&graph, SESSION);
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, RoutingAction::MoveStream { id: 7, .. }))
        );
        assert!(actions.contains(&RoutingAction::MuteSink("speaker".into())));
        graph.sinks[0].muted = true;
        graph.default_sink = Some(SESSION.into());
        assert!(enforce(&graph, SESSION).is_empty());
        assert!(
            evidence(&graph, SESSION, true).honoured,
            "a stream on a muted sink is silent"
        );
    }

    #[test]
    fn a_compliant_graph_needs_nothing_and_is_evidence() {
        let graph = PlaybackGraph {
            default_sink: Some(SESSION.into()),
            sinks: vec![sink("speaker", true), sink(SESSION, false)],
            streams: vec![
                stream(7, Some(SESSION), false, false),
                stream(9, Some("speaker"), true, true),
            ],
        };
        assert!(enforce(&graph, SESSION).is_empty());
        let evidence = evidence(&graph, SESSION, true);
        assert_eq!(evidence.observed_muted, Some(true));
        assert!(evidence.honoured);
    }

    #[test]
    fn evidence_refuses_an_audible_sink_a_wrong_default_or_a_missing_session_sink() {
        let graph = speaker_graph();
        assert_eq!(evidence(&graph, SESSION, true).observed_muted, Some(false));
        let mut graph = speaker_graph();
        graph.sinks[0].muted = true;
        graph.streams[0].target = StreamTarget::Sink(SESSION.into());
        assert!(
            !evidence(&graph, SESSION, true).honoured,
            "default is wrong"
        );
        graph.default_sink = Some(SESSION.into());
        assert!(evidence(&graph, SESSION, true).honoured);
        let missing = evidence(&graph, "arcen_session_other", true);
        assert_eq!(missing.observed_muted, None);
        assert!(!missing.honoured);
    }

    #[test]
    fn a_graph_without_the_session_sink_gets_no_actions() {
        assert!(enforce(&speaker_graph(), "absent").is_empty());
    }

    #[test]
    fn enforcement_converges() {
        let mut graph = speaker_graph();
        for action in enforce(&graph, SESSION) {
            match action {
                RoutingAction::MuteSink(name) => {
                    graph
                        .sinks
                        .iter_mut()
                        .find(|sink| sink.name == name)
                        .unwrap()
                        .muted = true;
                }
                RoutingAction::SetDefault(name) => graph.default_sink = Some(name),
                RoutingAction::MoveStream { id, to } => {
                    graph
                        .streams
                        .iter_mut()
                        .find(|stream| stream.id == id)
                        .unwrap()
                        .target = StreamTarget::Sink(to);
                }
            }
        }
        assert!(enforce(&graph, SESSION).is_empty());
        assert!(evidence(&graph, SESSION, true).honoured);
    }

    #[test]
    fn restore_undoes_only_what_the_lease_changed() {
        let mut journal = LeaseJournal::new(SESSION, Some("speaker".into()));
        journal.module_id = Some(42);
        for action in enforce(&speaker_graph(), SESSION) {
            action.record(&mut journal);
        }
        assert_eq!(journal.muted_sinks, BTreeSet::from(["speaker".to_owned()]));

        let during = PlaybackGraph {
            default_sink: Some(SESSION.into()),
            sinks: vec![
                sink("speaker", true),
                sink("headset", true),
                sink(SESSION, false),
            ],
            streams: vec![stream(11, Some("speaker"), true, true)],
        };
        assert_eq!(
            restore(&journal, &during, &[42]),
            vec![
                RestoreAction::SetDefault("speaker".into()),
                RestoreAction::UnmuteSink("speaker".into()),
                RestoreAction::UnloadModule(42),
            ],
            "the headset the user muted stays muted"
        );
    }

    #[test]
    fn restore_respects_a_default_chosen_since_and_vanished_objects() {
        let mut journal = LeaseJournal::new(SESSION, Some("speaker".into()));
        journal.muted_sinks.insert("gone".into());
        let graph = PlaybackGraph {
            default_sink: Some("headset".into()),
            sinks: vec![sink("speaker", false), sink("headset", false)],
            streams: Vec::new(),
        };
        assert!(restore(&journal, &graph, &[]).is_empty());
        let no_prior = LeaseJournal::new(SESSION, None);
        assert!(restore(&no_prior, &PlaybackGraph::default(), &[]).is_empty());
    }

    fn session_sink_graph() -> PlaybackGraph {
        PlaybackGraph {
            default_sink: Some(SESSION.into()),
            sinks: vec![sink(SESSION, false)],
            streams: Vec::new(),
        }
    }

    #[test]
    fn a_session_sink_already_gone_unloads_nothing() {
        let mut journal = LeaseJournal::new(SESSION, None);
        journal.module_id = Some(42);
        let gone = PlaybackGraph {
            default_sink: Some("speaker".into()),
            sinks: vec![sink("speaker", false)],
            streams: Vec::new(),
        };
        assert!(restore(&journal, &gone, &[42]).is_empty());
        assert!(restore(&journal, &gone, &[]).is_empty());
    }

    #[test]
    fn a_reused_module_id_is_never_unloaded() {
        // The audio server restarted: id 42 now names someone else's module,
        // and the session sink (if it exists) belongs to module 57.
        let mut journal = LeaseJournal::new(SESSION, None);
        journal.module_id = Some(42);
        assert_eq!(
            restore(&journal, &session_sink_graph(), &[57]),
            vec![RestoreAction::UnloadModule(57)]
        );
        assert!(restore(&journal, &session_sink_graph(), &[]).is_empty());
        // A module whose id was never recorded is found by its sink's name.
        journal.module_id = None;
        assert_eq!(
            restore(&journal, &session_sink_graph(), &[57]),
            vec![RestoreAction::UnloadModule(57)]
        );
    }

    #[test]
    fn a_stream_on_an_unknown_sink_is_not_silence() {
        let mut graph = session_sink_graph();
        graph.sinks.push(sink("speaker", true));
        graph.streams = vec![Stream {
            id: 5,
            target: StreamTarget::Unknown,
            muted: false,
            move_refused: false,
        }];
        assert!(!graph.is_settled());
        let evidence_now = evidence(&graph, SESSION, true);
        assert_eq!(evidence_now.observed_muted, Some(false));
        assert!(!evidence_now.honoured);
        assert_eq!(
            enforce(&graph, SESSION),
            vec![RoutingAction::MoveStream {
                id: 5,
                to: SESSION.into()
            }],
            "a stream whose sink is unknown is moved like any other"
        );
        // A named sink the reading does not list is just as unknown.
        graph.streams[0].target = StreamTarget::Sink("hdmi".into());
        assert!(!graph.is_settled());
        assert!(!evidence(&graph, SESSION, true).honoured);
        // A stream that plays nowhere is silent.
        graph.streams[0].target = StreamTarget::Unconnected;
        assert!(graph.is_settled());
        assert!(evidence(&graph, SESSION, true).honoured);
    }

    #[test]
    fn journal_round_trips_and_refuses_unknown_versions() {
        let mut journal = LeaseJournal::new(SESSION, Some("speaker".into()));
        journal.module_id = Some(3);
        journal.muted_sinks.insert("speaker".into());
        let text = serde_json::to_string(&journal).unwrap();
        let back: LeaseJournal = serde_json::from_str(&text).unwrap();
        assert_eq!(back, journal);
        assert!(back.is_supported());
        let future = text.replace("\"version\":1", "\"version\":2");
        let future: LeaseJournal = serde_json::from_str(&future).unwrap();
        assert!(!future.is_supported());
        assert!(serde_json::from_str::<LeaseJournal>(&text.replace('}', ",\"x\":1}")).is_err());
    }

    #[test]
    fn shared_server_sink_names_are_bounded_and_plain() {
        assert_eq!(
            shared_server_session_sink("ab12").as_deref(),
            Some("arcen_session_ab12")
        );
        for bad in ["", "AB", "a-b", "a b", &"a".repeat(MAX_SINK_TAG_BYTES + 1)] {
            assert_eq!(shared_server_session_sink(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn wayland_audio_mode_follows_the_facts() {
        assert_eq!(
            resolve_wayland_audio(WaylandAudioFacts {
                private_pulse_server: true,
                pulse_tunnel_module: true
            }),
            (SessionAudioMode::PrivateGraph, None)
        );
        assert_eq!(
            resolve_wayland_audio(WaylandAudioFacts {
                private_pulse_server: false,
                pulse_tunnel_module: true
            }),
            (
                SessionAudioMode::BridgedToUserPulse,
                Some(SessionAudioDegradation::NoPrivatePulseServer)
            )
        );
        let (mode, degradation) = resolve_wayland_audio(WaylandAudioFacts::default());
        assert_eq!(mode, SessionAudioMode::UserPulseOnly);
        assert!(!mode.serves_native_pipewire());
        assert_eq!(
            degradation,
            Some(SessionAudioDegradation::NativePipeWireUnserved)
        );
        assert!(!SessionAudioMode::PrivateGraph.uses_shared_server());
        assert!(SessionAudioMode::BridgedToUserPulse.uses_shared_server());
    }

    #[test]
    fn tunnel_reconnect_needs_pipewire_1_1() {
        assert!(!pulse_tunnel_reconnects(0, 3));
        assert!(!pulse_tunnel_reconnects(1, 0));
        assert!(pulse_tunnel_reconnects(1, 1));
        assert!(pulse_tunnel_reconnects(1, 4));
        assert!(pulse_tunnel_reconnects(2, 0));
    }

    #[test]
    fn evidence_serialises_with_the_established_field_names() {
        let report = LocalPlaybackReport {
            evidence: MuteEvidence::observed(true, Some(true)),
            basis: MuteBasis::PrivateGraph,
        };
        let text = serde_json::to_string(&report).unwrap();
        assert_eq!(
            text,
            r#"{"evidence":{"requested":true,"observed_muted":true,"honoured":true},"basis":"private_graph"}"#
        );
    }
}
