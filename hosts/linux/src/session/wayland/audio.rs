//! Audio of a headless Wayland session.
//!
//! What to build is [`arcen_session::local_playback::resolve_wayland_audio`]'s
//! decision from facts found here: a private graph (a session sink in the
//! private `PipeWire` and a private PulseAudio-protocol server), a bridge into
//! the user's PulseAudio (a pulse tunnel in the private `PipeWire`), or
//! nothing (PulseAudio clients only). This module finds the facts, writes the
//! private daemon's drop-in, and holds the session's local-playback lease.

use std::path::{Path, PathBuf};

use arcen_session::local_playback::{
    admit, pulse_tunnel_reconnects, resolve_wayland_audio, shared_server_session_sink,
    LocalPlaybackReport, MuteBasis, SessionAudioDegradation, SessionAudioMode, WaylandAudioFacts,
    PRIVATE_SESSION_SINK, SESSION_SINK_DESCRIPTION,
};
use arcen_session::pier_config::LocalPlayback;

use crate::logging::target;
use crate::media::local_playback::{
    establish_shared_lease, PlaybackLease, PulseServer, SessionSink, JOURNAL_NAME,
};
use crate::media::pipewire_bridge::{pipewire_module_installed, PulseBridge};
use crate::session::agent::SessionAudioReady;

/// The private `PipeWire`'s configuration home (`XDG_CONFIG_HOME`).
pub const CONFIG_DIR: &str = "config";
/// The private `WirePlumber`'s state home (`XDG_STATE_HOME`).
pub const STATE_DIR: &str = "state";
/// The private PulseAudio-protocol server's runtime path.
pub const PULSE_DIR: &str = "pulse";
/// The drop-in that gives the private `PipeWire` its audio.
pub const DROP_IN_NAME: &str = "90-arcen-session-audio.conf";
/// The PulseAudio-protocol server configuration `pipewire -c` runs.
pub const PULSE_SERVER_CONFIG: &str = "pipewire-pulse.conf";
const CONFIG_DIRS: &[&str] = &["/etc/pipewire", "/usr/share/pipewire"];
/// The longest a lease on the user's own server may take before the
/// session's mute is judged unproven.
const LEASE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
const PULSE_PROTOCOL_MODULE: &str = "libpipewire-module-protocol-pulse.so";
const PULSE_TUNNEL_MODULE: &str = "libpipewire-module-pulse-tunnel.so";

/// What this host offers a private session graph.
#[must_use]
pub fn probe_facts() -> WaylandAudioFacts {
    WaylandAudioFacts {
        private_pulse_server: pipewire_module_installed(PULSE_PROTOCOL_MODULE)
            && CONFIG_DIRS
                .iter()
                .any(|directory| Path::new(directory).join(PULSE_SERVER_CONFIG).is_file()),
        pulse_tunnel_module: pipewire_module_installed(PULSE_TUNNEL_MODULE),
    }
}

/// What the private stack builds for audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackAudio {
    /// No audio in the private graph.
    None,
    /// A session sink and a private PulseAudio-protocol server.
    Private,
}

impl StackAudio {
    /// Whether the private daemon needs Arcen's drop-in.
    #[must_use]
    pub const fn has_drop_in(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether the stack runs a private PulseAudio-protocol server.
    #[must_use]
    pub const fn runs_pulse_server(&self) -> bool {
        matches!(self, Self::Private)
    }
}

/// The private daemon's drop-in for `audio`, or `None` when it needs none.
#[must_use]
pub fn pipewire_drop_in(audio: &StackAudio) -> Option<String> {
    match audio {
        StackAudio::None => None,
        StackAudio::Private => Some(format!(
            "# Arcen headless session: the session's own audio sink.\n\
             context.objects = [\n  {{ factory = adapter\n    args = {{\n      \
             factory.name     = support.null-audio-sink\n      \
             node.name        = \"{PRIVATE_SESSION_SINK}\"\n      \
             node.description = \"{SESSION_SINK_DESCRIPTION}\"\n      \
             media.class      = \"Audio/Sink\"\n      \
             audio.position   = [ FL FR ]\n      \
             audio.rate       = 48000\n      \
             priority.session = 2000\n    }}\n  }}\n]\n"
        )),
    }
}

/// The user's own configuration directory for `PipeWire`.
fn user_pipewire_config() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|config| config.join("pipewire"))
}

/// Writes the private configuration home: the user's own `PipeWire`
/// configuration, linked file by file, plus Arcen's drop-in. Drop-ins in
/// `$XDG_CONFIG_HOME` add to the system configuration rather than replacing
/// it (`PIPEWIRE_CONFIG_DIR` would replace it).
///
/// # Errors
///
/// When a directory, link or the drop-in cannot be written.
pub fn write_pipewire_config(
    config_home: &Path,
    audio: &StackAudio,
    link_tree: impl Fn(&Path, &Path, usize) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let Some(drop_in) = pipewire_drop_in(audio) else {
        return Ok(());
    };
    let pipewire = config_home.join("pipewire");
    let drop_ins = pipewire.join("pipewire.conf.d");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&drop_ins)?;
    if let Some(user) = user_pipewire_config() {
        link_tree(&user, &pipewire, 0)?;
    }
    let path = drop_ins.join(DROP_IN_NAME);
    let _ = std::fs::remove_file(&path);
    std::fs::write(path, drop_in)
}

/// The session's audio: how it is served, and its local-playback lease.
#[derive(Debug)]
pub struct WaylandSessionAudio {
    mode: SessionAudioMode,
    degradation: Option<SessionAudioDegradation>,
    policy: LocalPlayback,
    pactl: PathBuf,
    stack: StackAudio,
    lease: Option<PlaybackLease>,
    report: LocalPlaybackReport,
    capture_source: Option<String>,
    pulse_runtime_path: Option<PathBuf>,
    /// A bridged graph's user server socket and tunnel reconnect support.
    bridge_spec: Option<(PathBuf, bool)>,
    /// A bridged graph's tunnels, once the private graph exists.
    bridge: Option<PulseBridge>,
}

/// The session refused because its host could not be proven silent.
#[derive(Debug, Clone, Copy)]
pub struct LocalPlaybackRefused(pub LocalPlaybackReport);

impl WaylandSessionAudio {
    /// Decides the session's audio from the host's facts and, when the
    /// session sink lives in the user's own server, starts the lease there
    /// (before the private graph exists, so a bridge has its target).
    ///
    /// # Errors
    ///
    /// [`LocalPlaybackRefused`] when the policy requires a mute and the user's
    /// server cannot be proven silent.
    pub async fn prepare(
        policy: LocalPlayback,
        pactl: PathBuf,
        session_tag: &str,
        pipewire_version: Option<(u32, u32)>,
    ) -> Result<Self, LocalPlaybackRefused> {
        let (mode, degradation) = resolve_wayland_audio(probe_facts());
        let mut audio = Self {
            mode,
            degradation,
            policy,
            pactl,
            stack: StackAudio::None,
            lease: None,
            report: LocalPlaybackReport::not_requested(),
            capture_source: None,
            pulse_runtime_path: None,
            bridge_spec: None,
            bridge: None,
        };
        if let Some(degradation) = degradation {
            tracing::warn!(
                target: target::AUDIO,
                event = "session_audio_mode",
                mode = mode.token(),
                degradation = degradation.token(),
                "this host cannot give the session a private audio graph"
            );
        }
        match mode {
            SessionAudioMode::PrivateGraph => {
                audio.stack = StackAudio::Private;
                Ok(audio)
            }
            SessionAudioMode::BridgedToUserPulse | SessionAudioMode::UserPulseOnly => {
                let user_server = user_pulse_socket();
                let server = PulseServer::inherited(&audio.pactl);
                let sink = shared_server_session_sink(session_tag);
                if policy.requires_mute() {
                    audio.lease_shared(server, sink.clone()).await?;
                }
                if let SessionAudioMode::BridgedToUserPulse = mode {
                    audio.bridge_spec = Some((
                        user_server,
                        pipewire_version
                            .is_some_and(|(major, minor)| pulse_tunnel_reconnects(major, minor)),
                    ));
                }
                Ok(audio)
            }
            SessionAudioMode::UserServer => Ok(audio),
        }
    }

    async fn lease_shared(
        &mut self,
        server: PulseServer,
        sink: Option<String>,
    ) -> Result<(), LocalPlaybackRefused> {
        let journal = user_runtime_dir().join(JOURNAL_NAME);
        let outcome =
            match tokio::time::timeout(LEASE_BUDGET, establish_shared_lease(server, journal, sink))
                .await
            {
                Ok(outcome) => outcome,
                Err(_) => {
                    tracing::warn!(
                        target: target::AUDIO,
                        event = "local_playback_lease",
                        action = "timed_out",
                        "the user's audio server did not answer in time; the mute is unproven"
                    );
                    crate::media::local_playback::SharedLeaseOutcome {
                        lease: None,
                        report: LocalPlaybackReport {
                            evidence: arcen_session::local_playback::MuteEvidence::observed(
                                true, None,
                            ),
                            basis: MuteBasis::SessionSinkLease,
                        },
                    }
                }
            };
        self.report = outcome.report;
        self.capture_source = outcome
            .lease
            .as_ref()
            .map(|lease| format!("{}.monitor", lease.session_sink()));
        self.lease = outcome.lease;
        if admit(self.policy, Some(self.report.evidence)).is_err() {
            self.release().await;
            return Err(LocalPlaybackRefused(self.report));
        }
        Ok(())
    }

    /// What the private stack must build.
    #[must_use]
    pub const fn stack(&self) -> &StackAudio {
        &self.stack
    }

    /// Completes a private graph once its server answers at
    /// `runtime_dir/pulse`: the lease (for `muted`) or the bridge to the
    /// user's speakers (for `audible`).
    ///
    /// # Errors
    ///
    /// [`LocalPlaybackRefused`] when a required mute cannot be proven.
    pub async fn attach_private(&mut self, runtime_dir: &Path) -> Result<(), LocalPlaybackRefused> {
        if let Some((user_server, reconnect)) = self.bridge_spec.clone() {
            // Both tunnels are clients of the private daemon; the sink
            // tunnel plays into a muted session's lease sink, or follows the
            // user's default sink.
            let session_sink = self
                .lease
                .as_ref()
                .map(|lease| lease.session_sink().to_owned());
            self.bridge = Some(
                PulseBridge::start(
                    &PulseServer::inherited(&self.pactl),
                    &user_server,
                    runtime_dir,
                    Some(runtime_dir),
                    reconnect,
                    session_sink,
                )
                .await,
            );
        }
        if self.mode != SessionAudioMode::PrivateGraph {
            return Ok(());
        }
        let pulse = runtime_dir.join(PULSE_DIR);
        let server = PulseServer::at_socket(&self.pactl, &pulse.join("native"));
        self.pulse_runtime_path = Some(pulse.clone());
        self.capture_source = Some(format!("{PRIVATE_SESSION_SINK}.monitor"));
        if self.policy.requires_mute() {
            match PlaybackLease::establish(
                server,
                runtime_dir.join(JOURNAL_NAME),
                SessionSink::Existing(PRIVATE_SESSION_SINK.to_owned()),
                MuteBasis::PrivateGraph,
            )
            .await
            {
                Ok(lease) => {
                    self.report = lease.report().await;
                    self.lease = Some(lease);
                }
                Err(error) => {
                    tracing::warn!(target: target::AUDIO, %error, "the private session graph could not be proven silent");
                    self.report = LocalPlaybackReport {
                        evidence: arcen_session::local_playback::MuteEvidence::observed(true, None),
                        basis: MuteBasis::PrivateGraph,
                    };
                }
            }
            if admit(self.policy, Some(self.report.evidence)).is_err() {
                return Err(LocalPlaybackRefused(self.report));
            }
        } else {
            bridge_to_user_speakers(&server).await;
        }
        tracing::info!(
            target: target::AUDIO,
            event = "session_audio_mode",
            mode = self.mode.token(),
            local_playback = self.policy.token(),
            basis = self.report.basis.token(),
            honoured = self.report.evidence.honoured,
            "the session's audio is its own private graph"
        );
        Ok(())
    }

    /// The facts the Pier needs.
    #[must_use]
    pub fn ready(&self) -> SessionAudioReady {
        SessionAudioReady {
            mode: self.mode,
            pulse_runtime_path: self.pulse_runtime_path.clone(),
            capture_source: self.capture_source.clone(),
            local_playback: self.report,
            native_pipewire: self.mode.serves_native_pipewire(),
            degradation: self
                .degradation
                .map(|degradation| degradation.token().to_owned()),
        }
    }

    /// Keeps the lease enforced and, in a bridge, both tunnels on the
    /// user's defaults; pending forever when there is nothing to keep.
    pub async fn watch(&mut self) {
        let lease = async {
            match self.lease.as_mut() {
                Some(lease) => lease.watch().await,
                None => std::future::pending().await,
            }
        };
        let bridge = async {
            match self.bridge.as_mut() {
                Some(bridge) => bridge.follow().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::join!(lease, bridge);
    }

    /// Ends the lease, restoring what it changed.
    pub async fn release(&mut self) {
        if let Some(mut bridge) = self.bridge.take() {
            bridge.stop().await;
        }
        if let Some(lease) = self.lease.take() {
            let _ = lease.release().await;
        }
    }
}

/// Plays the session sink on the user's own default sink as well, for an
/// operator who chose `audible`. Failure is logged, never fatal: the
/// session is served either way and the operator's choice was audibility,
/// not a requirement.
async fn bridge_to_user_speakers(server: &PulseServer) {
    let user = user_pulse_socket();
    let tunnel = server
        .load_module(
            "module-tunnel-sink",
            &[
                format!("server=unix:{}", user.display()),
                "sink_name=arcen_host_speakers".to_owned(),
                "sink_properties=device.description=Host_speakers priority.session=0".to_owned(),
            ],
        )
        .await;
    let loopback = match &tunnel {
        Ok(_) => {
            server
                .load_module(
                    "module-loopback",
                    &[
                        format!("source={PRIVATE_SESSION_SINK}.monitor"),
                        "sink=arcen_host_speakers".to_owned(),
                        "latency_msec=60".to_owned(),
                    ],
                )
                .await
        }
        Err(_) => Err(crate::media::local_playback::LeaseError::Parse(
            "no tunnel to loop into",
        )),
    };
    let _ = server.set_default_sink(PRIVATE_SESSION_SINK).await;
    match (tunnel, loopback) {
        (Ok(_), Ok(_)) => tracing::info!(
            target: target::AUDIO,
            event = "local_playback_audible",
            "session audio also plays on the host's speakers, as configured"
        ),
        (tunnel, loopback) => tracing::warn!(
            target: target::AUDIO,
            event = "local_playback_audible",
            tunnel = ?tunnel.err().map(|error| error.to_string()),
            loopback = ?loopback.err().map(|error| error.to_string()),
            "local playback is configured audible but the host's speakers could not be reached"
        ),
    }
}

fn user_runtime_dir() -> PathBuf {
    PathBuf::from(format!(
        "/run/user/{}",
        nix::unistd::Uid::effective().as_raw()
    ))
}

/// The user's own PulseAudio-protocol socket.
fn user_pulse_socket() -> PathBuf {
    std::env::var_os("PULSE_RUNTIME_PATH")
        .filter(|value| !value.is_empty())
        .map_or_else(|| user_runtime_dir().join("pulse"), PathBuf::from)
        .join("native")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_private_drop_in_creates_exactly_the_session_sink() {
        let text = pipewire_drop_in(&StackAudio::Private).unwrap();
        assert!(text.contains("context.objects"));
        assert!(text.contains("factory.name     = support.null-audio-sink"));
        assert!(text.contains("node.name        = \"arcen_session_sink\""));
        assert!(text.contains("media.class      = \"Audio/Sink\""));
        assert!(!text.contains("pulse-tunnel"));
        assert_eq!(pipewire_drop_in(&StackAudio::None), None);
    }

    #[test]
    fn stack_audio_kinds() {
        assert!(!StackAudio::None.has_drop_in());
        assert!(!StackAudio::None.runs_pulse_server());
        assert!(StackAudio::Private.has_drop_in());
        assert!(StackAudio::Private.runs_pulse_server());
    }
}
