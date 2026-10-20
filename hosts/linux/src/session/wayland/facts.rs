//! Host facts for headless Wayland provider selection.
//!
//! Probes the actual binaries and device nodes; package presence alone is
//! never a fact. GPU import is never established here: only the capture
//! side can prove it imports compositor buffers.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use arcen_session::desktop_provider::CapabilityFact;
use arcen_session::wayland_outputs::{parse_compositor_major, VirtualOutputMode};
use arcen_session::wayland_session::{
    CompositorProbe, HeadlessCompositor, WaylandHostProbe, DBUS_DAEMON_BINARY as DBUS_DAEMON,
    LOGIND_SEATS_DIR as LOGIND_RUNTIME, PIPEWIRE_BINARY as PIPEWIRE,
    WIREPLUMBER_BINARY as WIREPLUMBER,
};
use tokio::process::Command;

use super::{
    choose_from_outcomes, compositor_binary, parse_compositor_help, CompositorChoiceError,
    ProbeOutcome,
};

const HELP_TIMEOUT: Duration = Duration::from_secs(5);
const DRI: &str = "/dev/dri";

fn executable(path: &str) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Runs `<compositor> --help` and reads which modes it advertises.
pub async fn probe_compositor(compositor: HeadlessCompositor) -> Option<CompositorProbe> {
    let binary = compositor_binary(compositor);
    if !executable(&binary) {
        return None;
    }
    let mut command = Command::new(&binary);
    super::unprivileged_probe(&mut command, &binary);
    let output = command
        .arg("--help")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(HELP_TIMEOUT, output)
        .await
        .ok()?
        .ok()?;
    let (headless, virtual_monitor) =
        parse_compositor_help(&String::from_utf8_lossy(&output.stdout));
    Some(CompositorProbe {
        compositor,
        headless,
        virtual_monitor,
    })
}

/// Probes every supported compositor.
pub async fn probe_compositors() -> Vec<CompositorProbe> {
    let mut probes = Vec::new();
    for compositor in [HeadlessCompositor::GnomeShell, HeadlessCompositor::Mutter] {
        probes.extend(probe_compositor(compositor).await);
    }
    probes
}

/// The output mode `compositor` supports, from its own `--version`
/// (`GNOME Shell 49.4`, `mutter 49.4`); an unreadable version is the fixed
/// mode.
pub async fn output_mode(compositor: HeadlessCompositor) -> VirtualOutputMode {
    let binary = compositor_binary(compositor);
    let mut command = Command::new(&binary);
    super::unprivileged_probe(&mut command, &binary);
    let output = command
        .arg("--version")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let major = tokio::time::timeout(HELP_TIMEOUT, output)
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|output| parse_compositor_major(&String::from_utf8_lossy(&output.stdout)));
    let mode = VirtualOutputMode::for_compositor_major(major);
    tracing::info!(
        target: crate::logging::target::SESSION,
        compositor = compositor.token(),
        major = ?major,
        mode = mode.token(),
        "headless compositor output mode"
    );
    mode
}

/// Chooses the session's compositor in preference order. A probe that fails
/// for an installed compositor stops the choice only if that compositor
/// would have been chosen — a slow or failing `gnome-shell --help` never
/// silently selects `mutter`, but a failing `mutter` probe never blocks a
/// usable `gnome-shell`.
///
/// # Errors
///
/// [`CompositorChoiceError`].
pub async fn choose_installed_compositor() -> Result<HeadlessCompositor, CompositorChoiceError> {
    let mut outcomes = Vec::new();
    for compositor in [HeadlessCompositor::GnomeShell, HeadlessCompositor::Mutter] {
        let outcome = if executable(&compositor_binary(compositor)) {
            probe_compositor(compositor)
                .await
                .map_or(ProbeOutcome::Failed, ProbeOutcome::Probed)
        } else {
            ProbeOutcome::NotInstalled
        };
        outcomes.push((compositor, outcome));
    }
    choose_from_outcomes(&outcomes)
}

/// Whether an unprivileged user can open a render node: some
/// `/dev/dri/renderD*` grants read and write to everyone. Group-only access
/// would need group membership Arcen never adds, so it is reported unknown.
fn render_node_fact() -> CapabilityFact {
    let Ok(entries) = std::fs::read_dir(DRI) else {
        return CapabilityFact::Missing;
    };
    let mut any = false;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("renderD") {
            continue;
        }
        any = true;
        if entry
            .metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o006 == 0o006)
        {
            return CapabilityFact::Available;
        }
    }
    if any {
        CapabilityFact::Unknown
    } else {
        CapabilityFact::Missing
    }
}

/// Gathers the host facts. Input injection stays unknown until a session's
/// `RemoteDesktop` target proves it; the capture route and GPU import until
/// the capture side does.
pub async fn probe_host() -> WaylandHostProbe {
    WaylandHostProbe {
        compositors: probe_compositors().await,
        logind: Path::new(LOGIND_RUNTIME).is_dir(),
        dbus_daemon: executable(DBUS_DAEMON),
        pipewire: executable(PIPEWIRE),
        session_manager: executable(WIREPLUMBER),
        render_node: render_node_fact(),
        capture_route: CapabilityFact::Unknown,
        gpu_import: CapabilityFact::Unknown,
        input_injection: CapabilityFact::Unknown,
    }
}
