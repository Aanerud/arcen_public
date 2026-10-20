//! A PipeWire graph bridged into a PulseAudio server with `PipeWire`'s pulse
//! tunnel, for hosts whose audio server is PulseAudio itself.
//!
//! Native `PipeWire` clients otherwise reach a `PipeWire` with no audio sink
//! there, and the Deck microphone (a PulseAudio pipe source) is invisible to
//! them. Two tunnels fix both: a sink tunnel that plays into the PulseAudio
//! default sink (the session sink while a local-playback lease holds it),
//! and a microphone tunnel that records the PulseAudio default source.
//!
//! `PipeWire` connects a tunnel's stream with `PA_STREAM_DONT_MOVE`: when
//! its PulseAudio sink or source goes away the stream dies instead of
//! moving, and it can never be moved to a new default. So each tunnel is
//! its own small `pipewire -c` client of the session's `PipeWire`, and a
//! [`TunnelFollower`] restarts it on the new default whenever the default
//! changes (a new Deck microphone generation, a sink that came or went).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use arcen_session::local_playback::{
    BRIDGE_TUNNEL_LATENCY_MS, BRIDGE_TUNNEL_RECONNECT_MS, PRIVATE_SESSION_SINK,
    SESSION_SINK_DESCRIPTION,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::logging::target;
use crate::media::local_playback::{parse_default_source, PulseServer};

/// The bridged microphone node.
pub const BRIDGED_MICROPHONE: &str = "arcen_session_microphone";
/// The sink bridge client's configuration, in the user's runtime directory.
pub const BRIDGE_CONFIG_NAME: &str = "arcen-audio-bridge.conf";
/// The microphone bridge client's configuration.
pub const MICROPHONE_BRIDGE_CONFIG_NAME: &str = "arcen-microphone-bridge.conf";
const PIPEWIRE: &str = "/usr/bin/pipewire";
const EVENT_SETTLE: Duration = Duration::from_millis(50);
const RESUBSCRIBE_MIN: Duration = Duration::from_millis(250);
const RESUBSCRIBE_MAX: Duration = Duration::from_secs(4);

const MODULE_DIRS: &[&str] = &[
    "/usr/lib64/pipewire-0.3",
    "/usr/lib/pipewire-0.3",
    "/usr/lib/x86_64-linux-gnu/pipewire-0.3",
    "/usr/lib/aarch64-linux-gnu/pipewire-0.3",
];

/// Whether `PipeWire` has `module` (`libpipewire-module-….so`) installed.
#[must_use]
pub fn pipewire_module_installed(module: &str) -> bool {
    MODULE_DIRS
        .iter()
        .any(|directory| Path::new(directory).join(module).is_file())
}

/// Whether `PipeWire` itself is installed.
#[must_use]
pub fn pipewire_installed() -> bool {
    Path::new(PIPEWIRE).is_file()
}

/// Whether the pulse tunnel is installed.
#[must_use]
pub fn pulse_tunnel_installed() -> bool {
    pipewire_module_installed("libpipewire-module-pulse-tunnel.so")
}

fn reconnect_line(reconnect: bool) -> String {
    if reconnect {
        format!("      reconnect.interval.ms = {BRIDGE_TUNNEL_RECONNECT_MS}\n")
    } else {
        String::new()
    }
}

fn target_line(target: Option<&str>) -> String {
    target
        .map(|name| format!("      target.object = \"{name}\"\n"))
        .unwrap_or_default()
}

/// The sink tunnel, as a `context.modules` entry. `target_sink` names the
/// PulseAudio sink it plays into (the server's default when `None`);
/// `reconnect` only where `PipeWire` supports it (1.1.0 and later).
#[must_use]
pub fn sink_tunnel_module(
    user_server: &Path,
    target_sink: Option<&str>,
    reconnect: bool,
) -> String {
    format!(
        "  {{ name = libpipewire-module-pulse-tunnel\n    args = {{\n      \
         tunnel.mode = sink\n      \
         pulse.server.address = \"unix:{server}\"\n{target}      \
         pulse.latency = {BRIDGE_TUNNEL_LATENCY_MS}\n{reconnect}      \
         audio.position = [ FL FR ]\n      \
         stream.props = {{\n        node.name = \"{PRIVATE_SESSION_SINK}\"\n        \
         node.description = \"{SESSION_SINK_DESCRIPTION}\"\n        \
         priority.session = 2000\n      }}\n    }}\n    flags = [ nofail ]\n  }}\n",
        server = user_server.display(),
        target = target_line(target_sink),
        reconnect = reconnect_line(reconnect),
    )
}

/// The microphone tunnel, recording `source` of the PulseAudio server.
#[must_use]
pub fn source_tunnel_module(user_server: &Path, source: &str, reconnect: bool) -> String {
    format!(
        "  {{ name = libpipewire-module-pulse-tunnel\n    args = {{\n      \
         tunnel.mode = source\n      \
         pulse.server.address = \"unix:{server}\"\n{target}      \
         pulse.latency = {BRIDGE_TUNNEL_LATENCY_MS}\n{reconnect}      \
         audio.channels = 1\n      \
         stream.props = {{\n        node.name = \"{BRIDGED_MICROPHONE}\"\n        \
         node.description = \"Arcen microphone\"\n        \
         priority.session = 2000\n      }}\n    }}\n    flags = [ nofail ]\n  }}\n",
        server = user_server.display(),
        target = target_line(Some(source)),
        reconnect = reconnect_line(reconnect),
    )
}

/// A complete configuration for a `pipewire -c` client process that hosts
/// `modules` in someone else's daemon.
#[must_use]
pub fn bridge_client_config(name: &str, modules: &str) -> String {
    format!(
        "# Arcen session audio bridge: PipeWire clients and PulseAudio.\n\
         context.properties = {{\n    core.daemon = false\n    core.name = {name}\n}}\n\
         context.spa-libs = {{\n    audio.convert.* = audioconvert/libspa-audioconvert\n    \
         support.*       = support/libspa-support\n}}\n\
         context.modules = [\n  \
         {{ name = libpipewire-module-rt flags = [ ifexists nofail ] }}\n  \
         {{ name = libpipewire-module-protocol-native }}\n  \
         {{ name = libpipewire-module-client-node }}\n  \
         {{ name = libpipewire-module-adapter }}\n{modules}]\n"
    )
}

/// `major.minor` of a `PipeWire` version string (`1.4.11` → `(1, 4)`).
#[must_use]
pub fn parse_major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The installed `PipeWire` library's `major.minor`.
pub async fn pipewire_version() -> Option<(u32, u32)> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(PIPEWIRE)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    arcen_session::wayland_qualification::parse_pipewire_version(&String::from_utf8_lossy(
        &output.stdout,
    ))
    .and_then(|version| parse_major_minor(&version))
}

/// Whether `pactl info` text describes PulseAudio itself rather than
/// `pipewire-pulse` (whose server name says "on PipeWire").
#[must_use]
pub fn server_is_pulseaudio_itself(info: &str) -> bool {
    info.lines()
        .find_map(|line| line.strip_prefix("Server Name: "))
        .is_some_and(|name| !name.contains("PipeWire"))
}

/// Which PulseAudio source the microphone tunnel should record: the
/// default, unless that is a sink's monitor (recording the session's own
/// output back as a microphone would be an echo).
#[must_use]
pub fn microphone_target(default_source: Option<&str>) -> Option<&str> {
    default_source.filter(|source| !source.ends_with(".monitor"))
}

/// A running bridge client. Dropping it stops the process.
#[derive(Debug)]
pub struct BridgeProcess {
    child: Child,
    config: PathBuf,
}

impl BridgeProcess {
    /// Writes `config_text` to `config` and runs `pipewire -c` on it,
    /// connecting to the daemon of `pipewire_runtime` (`XDG_RUNTIME_DIR`), or
    /// this process's own when `None`.
    ///
    /// # Errors
    ///
    /// When the configuration cannot be written or the process not started.
    pub fn start(
        config: PathBuf,
        config_text: &str,
        pipewire_runtime: Option<&Path>,
    ) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(&config);
        std::fs::write(&config, config_text)?;
        let mut command = Command::new(PIPEWIRE);
        command
            .arg("-c")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(runtime) = pipewire_runtime {
            command.env("XDG_RUNTIME_DIR", runtime);
        }
        crate::media::local_playback::tie_to_parent(&mut command);
        let child = command.spawn()?;
        Ok(Self { child, config })
    }

    /// Whether the process still runs.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stops the bridge and removes its configuration.
    pub async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        let _ = std::fs::remove_file(&self.config);
    }
}

/// Which way a tunnel carries audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelKind {
    /// `PipeWire` clients play into the PulseAudio default sink.
    Sink,
    /// `PipeWire` clients record the PulseAudio default source.
    Microphone,
}

impl TunnelKind {
    const fn token(self) -> &'static str {
        match self {
            Self::Sink => "sink",
            Self::Microphone => "microphone",
        }
    }

    /// The PulseAudio object this tunnel should be connected to, from
    /// `pactl info`.
    #[must_use]
    pub fn wanted(self, info: &str) -> Option<String> {
        match self {
            Self::Sink => crate::media::local_playback::parse_default_sink(info),
            Self::Microphone => {
                microphone_target(parse_default_source(info).as_deref()).map(str::to_owned)
            }
        }
    }

    /// Whether a `pactl subscribe` line can change [`Self::wanted`] or end
    /// the tunnel's stream.
    #[must_use]
    pub fn event_matters(self, line: &str) -> bool {
        let Some(rest) = line.strip_prefix("Event '") else {
            return false;
        };
        let Some((kind, rest)) = rest.split_once("' on ") else {
            return false;
        };
        let facility = rest.split(" #").next().unwrap_or(rest).trim();
        let wanted = match self {
            Self::Sink => "sink",
            Self::Microphone => "source",
        };
        matches!(kind, "new" | "change" | "remove") && (facility == wanted || facility == "server")
    }

    fn module(self, user_server: &Path, target: &str, reconnect: bool) -> String {
        match self {
            Self::Sink => sink_tunnel_module(user_server, Some(target), reconnect),
            Self::Microphone => source_tunnel_module(user_server, target, reconnect),
        }
    }
}

/// One tunnel, kept on the PulseAudio default of its kind.
#[derive(Debug)]
pub struct TunnelFollower {
    kind: TunnelKind,
    server: PulseServer,
    user_server: PathBuf,
    config: PathBuf,
    pipewire_runtime: Option<PathBuf>,
    reconnect: bool,
    /// A target that never changes (a muted session's lease sink), instead
    /// of the server's default.
    pinned: Option<String>,
    current: Option<(String, BridgeProcess)>,
}

impl TunnelFollower {
    /// A tunnel from `server` (whose socket is `user_server`) into the
    /// `PipeWire` of `pipewire_runtime` (this process's own when `None`),
    /// with its configuration in `config_dir`.
    #[must_use]
    pub fn new(
        kind: TunnelKind,
        server: PulseServer,
        user_server: PathBuf,
        config_dir: &Path,
        pipewire_runtime: Option<PathBuf>,
        reconnect: bool,
    ) -> Self {
        let config = config_dir.join(match kind {
            TunnelKind::Sink => BRIDGE_CONFIG_NAME,
            TunnelKind::Microphone => MICROPHONE_BRIDGE_CONFIG_NAME,
        });
        Self {
            kind,
            server,
            user_server,
            config,
            pipewire_runtime,
            reconnect,
            pinned: None,
            current: None,
        }
    }

    /// The same tunnel, always connected to `target` rather than the
    /// server's default: a lease that holds the default on its session sink
    /// would otherwise race the follower whenever something else briefly
    /// becomes the default.
    #[must_use]
    pub fn pinned_to(mut self, target: Option<String>) -> Self {
        self.pinned = target;
        self
    }

    /// Follows the default until the future is dropped.
    pub async fn follow(&mut self) {
        let mut retry = RESUBSCRIBE_MIN;
        loop {
            if let Ok(mut child) = self.server.subscribe_command().spawn() {
                if let Some(stdout) = child.stdout.take() {
                    retry = RESUBSCRIBE_MIN;
                    self.realign().await;
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if self.kind.event_matters(&line) {
                            tokio::time::sleep(EVENT_SETTLE).await;
                            self.realign().await;
                        }
                    }
                }
                let _ = child.start_kill();
            }
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(RESUBSCRIBE_MAX);
        }
    }

    /// Connects the tunnel to the current default, restarting it when the
    /// default changed or its process ended.
    pub async fn realign(&mut self) {
        let Ok(info) = self.server.info().await else {
            return;
        };
        let wanted = self.pinned.clone().or_else(|| self.kind.wanted(&info));
        let alive = self
            .current
            .as_mut()
            .is_some_and(|(_, bridge)| bridge.is_running());
        if alive && self.current.as_ref().map(|(target, _)| target) == wanted.as_ref() {
            return;
        }
        if let Some((_, bridge)) = self.current.take() {
            bridge.stop().await;
        }
        let Some(target) = wanted else {
            return;
        };
        let modules = self.kind.module(&self.user_server, &target, self.reconnect);
        match BridgeProcess::start(
            self.config.clone(),
            &bridge_client_config("arcen-audio-bridge", &modules),
            self.pipewire_runtime.as_deref(),
        ) {
            Ok(bridge) => {
                tracing::info!(
                    target: target::AUDIO,
                    event = "session_audio_bridge",
                    tunnel = self.kind.token(),
                    target = %target,
                    "a PipeWire tunnel into the user's PulseAudio follows its default"
                );
                self.current = Some((target, bridge));
            }
            Err(error) => tracing::warn!(
                target: target::AUDIO,
                tunnel = self.kind.token(),
                %error,
                "a PipeWire tunnel into the user's PulseAudio could not start"
            ),
        }
    }

    /// Stops the tunnel.
    pub async fn stop(&mut self) {
        if let Some((_, bridge)) = self.current.take() {
            bridge.stop().await;
        }
    }
}

/// Both tunnels of a bridged session.
#[derive(Debug)]
pub struct PulseBridge {
    tunnels: [TunnelFollower; 2],
}

impl PulseBridge {
    /// Starts the sink and microphone tunnels from `server` (whose socket
    /// is `user_server`) into the `PipeWire` of `pipewire_runtime` (this
    /// process's own when `None`), connected to the current defaults; the
    /// sink tunnel stays on `session_sink` when a lease holds one.
    pub async fn start(
        server: &PulseServer,
        user_server: &Path,
        config_dir: &Path,
        pipewire_runtime: Option<&Path>,
        reconnect: bool,
        session_sink: Option<String>,
    ) -> Self {
        let follower = |kind| {
            TunnelFollower::new(
                kind,
                server.clone(),
                user_server.to_path_buf(),
                config_dir,
                pipewire_runtime.map(Path::to_path_buf),
                reconnect,
            )
        };
        let mut bridge = Self {
            tunnels: [
                follower(TunnelKind::Sink).pinned_to(session_sink),
                follower(TunnelKind::Microphone),
            ],
        };
        for tunnel in &mut bridge.tunnels {
            tunnel.realign().await;
        }
        bridge
    }

    /// Keeps both tunnels on their defaults until the future is dropped.
    pub async fn follow(&mut self) {
        let [sink, microphone] = &mut self.tunnels;
        tokio::join!(sink.follow(), microphone.follow());
    }

    /// Stops both tunnels.
    pub async fn stop(&mut self) {
        for tunnel in &mut self.tunnels {
            tunnel.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "/run/user/1000/pulse/native";

    #[test]
    fn the_sink_tunnel_plays_into_the_session_sink() {
        let text = sink_tunnel_module(Path::new(SERVER), Some("arcen_session_ab12"), false);
        assert_eq!(text.matches("libpipewire-module-pulse-tunnel").count(), 1);
        assert!(text.contains("tunnel.mode = sink"));
        assert!(text.contains("pulse.server.address = \"unix:/run/user/1000/pulse/native\""));
        assert!(text.contains("target.object = \"arcen_session_ab12\""));
        assert!(text.contains("node.name = \"arcen_session_sink\""));
        assert!(text.contains("pulse.latency = 40"));
        assert!(!text.contains("reconnect.interval.ms"));
        assert!(text.contains("flags = [ nofail ]"));
        let text = sink_tunnel_module(Path::new(SERVER), None, true);
        assert!(text.contains("reconnect.interval.ms = 1000"));
        assert!(!text.contains("target.object"));
    }

    #[test]
    fn the_microphone_tunnel_records_its_source() {
        let text = source_tunnel_module(Path::new(SERVER), "arcen_microphone_3", false);
        assert!(text.contains("tunnel.mode = source"));
        assert!(text.contains("target.object = \"arcen_microphone_3\""));
        assert!(text.contains("node.name = \"arcen_session_microphone\""));
        assert!(text.contains("audio.channels = 1"));
    }

    #[test]
    fn a_bridge_client_is_not_a_daemon_and_hosts_only_its_tunnel() {
        let text = bridge_client_config(
            "arcen-audio-bridge",
            &sink_tunnel_module(Path::new(SERVER), None, false),
        );
        assert!(text.contains("core.daemon = false"));
        assert!(text.contains("libpipewire-module-protocol-native"));
        assert!(text.contains("libpipewire-module-adapter"));
        assert_eq!(text.matches("libpipewire-module-pulse-tunnel").count(), 1);
        assert!(!text.contains("protocol-pulse"));
    }

    #[test]
    fn the_microphone_never_records_a_monitor() {
        assert_eq!(
            microphone_target(Some("arcen_microphone_2")),
            Some("arcen_microphone_2")
        );
        assert_eq!(microphone_target(Some("arcen_session_ab.monitor")), None);
        assert_eq!(microphone_target(None), None);
        let microphone = TunnelKind::Microphone;
        assert!(microphone.event_matters("Event 'change' on server"));
        assert!(microphone.event_matters("Event 'remove' on source #4"));
        assert!(!microphone.event_matters("Event 'new' on sink #4"));
        assert!(!microphone.event_matters("Event 'new' on client #4"));
        let sink = TunnelKind::Sink;
        assert!(sink.event_matters("Event 'remove' on sink #4"));
        assert!(!sink.event_matters("Event 'new' on sink-input #4"));
        assert!(!sink.event_matters("Event 'new' on source #4"));
        let info = include_str!("../../tests/fixtures/pactl/pulseaudio-15/info.txt");
        assert_eq!(sink.wanted(info).as_deref(), Some("lab_speaker"));
        assert_eq!(
            microphone.wanted(info),
            None,
            "its default source is a monitor"
        );
    }

    #[test]
    fn pipewire_versions_parse() {
        assert_eq!(parse_major_minor("1.4.11"), Some((1, 4)));
        assert_eq!(parse_major_minor("1.0.1"), Some((1, 0)));
        assert_eq!(parse_major_minor("x"), None);
    }

    #[test]
    fn pulseaudio_itself_is_told_from_pipewire_pulse() {
        assert!(server_is_pulseaudio_itself(include_str!(
            "../../tests/fixtures/pactl/pulseaudio-15/info.txt"
        )));
        assert!(!server_is_pulseaudio_itself(include_str!(
            "../../tests/fixtures/pactl/pipewire-pulse-1.4/info.txt"
        )));
        assert!(!server_is_pulseaudio_itself("Server String: x\n"));
    }
}
