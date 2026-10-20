//! Concurrent multi-output qualification.
//!
//! One headless session opened with several virtual outputs, every output
//! captured and encoded by its own capenc at once, as a multi-monitor
//! attachment does: it proves the compositor serves the outputs side by
//! side and the encoder opens one session per output, and records what each
//! output's source delivered and its encoder produced. Only host-side
//! evidence is recorded (capenc's per-second `source_fps`/`enc_fps`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use arcen_media::video::{CopyPath, SustainedRateMeter};
use arcen_session::desktop_provider::{CapabilityFact, DesktopProvider};
use arcen_session::wayland_qualification::{CaptureRoute, MultiOutputRun, OutputRateMeasurement};

use super::{leave_overview, session_gone, step, MotionDriver, QualificationUser, Run};
use crate::media::capenc::{self, PipelineStatsSample};
use crate::session::launcher::{AuthenticatedLauncher, LauncherConfig};

/// Outputs measured together when the operator names no count.
pub(super) const DEFAULT_MULTI_OUTPUTS: usize = 4;
/// The display each concurrent output runs when the operator names none.
pub(super) const DEFAULT_MULTI_OUTPUT: (u32, u32, u32) = (1920, 1080, 60);

/// How long the pointer moves on each output: enough for the output's rate
/// to hold over [`SustainedRateMeter::WINDOWS`] consecutive seconds.
const SECONDS_PER_OUTPUT: u32 = 5;

/// The topology a multi-monitor sign-in of `count` displays of one size asks
/// for: side by side, the first primary.
fn side_by_side(
    count: usize,
    (width, height, refresh_hz): (u32, u32, u32),
) -> Result<crate::display::topology::LinuxTopologyPlan, String> {
    let monitors = (0..count)
        .map(|index| {
            let x = i32::try_from(index)
                .ok()
                .and_then(|index| index.checked_mul(i32::try_from(width).ok()?))
                .ok_or("output layout overflows")?;
            arcen_media::RequestedMonitor::new(
                arcen_media::Monitor {
                    identity: arcen_media::MonitorIdentity {
                        id: format!("qualify-{index}"),
                        ..arcen_media::MonitorIdentity::default()
                    },
                    x,
                    y: 0,
                    width_px: width,
                    height_px: height,
                    scale: 1.0,
                    refresh_hz,
                    rotation: arcen_media::Rotation::Degrees0,
                    primary: index == 0,
                    width_mm: 0.0,
                    height_mm: 0.0,
                    color: None,
                },
                width,
                height,
            )
            .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, String>>()?;
    let requested =
        arcen_media::RequestedMonitorTopology::new(monitors).map_err(|error| error.to_string())?;
    crate::display::topology::plan_virtual_topology(
        &requested,
        arcen_media::TopologyGeneration::new(1).map_err(|error| error.to_string())?,
        crate::display::topology::wayland_output_limits(count),
        arcen_session::wayland_outputs::VirtualOutputMode::RemoteMonitors,
    )
    .map_err(|error| error.to_string())
}

/// Best sustained source and encoded rates over `seconds` of one output's
/// per-second stats lines.
async fn measure(
    mut stats: tokio::sync::watch::Receiver<Option<PipelineStatsSample>>,
    seconds: u32,
) -> (Option<u32>, Option<u32>, Vec<(Option<u32>, u32)>) {
    let start = Instant::now();
    let mut source_meter = SustainedRateMeter::new(start);
    let mut encoded_meter = SustainedRateMeter::new(start);
    let mut samples = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(seconds));
    while let Ok(Ok(())) = tokio::time::timeout_at(deadline, stats.changed()).await {
        let Some(sample) = *stats.borrow_and_update() else {
            continue;
        };
        let at = start + Duration::from_secs(samples.len() as u64);
        if let Some(source) = sample.source {
            source_meter.record(at, source.min(sample.encoded));
        }
        encoded_meter.record(at, sample.encoded);
        samples.push((sample.source, sample.encoded));
    }
    let end = start + Duration::from_secs(samples.len() as u64);
    source_meter.tick(end);
    encoded_meter.tick(end);
    (
        source_meter.best_sustained(),
        encoded_meter.best_sustained(),
        samples,
    )
}

/// Opens one session with `count` outputs of `output`'s size side by side,
/// runs one capenc per output concurrently while the pointer moves on each
/// output in turn, and reports what every output delivered; `None` when the
/// session or a capture did not start.
pub(super) async fn qualify_multi_output(
    config: &crate::cli::Config,
    user: &QualificationUser,
    output: (u32, u32, u32),
    count: usize,
    gpu_import_verified: bool,
    native_encoder: CapabilityFact,
) -> Option<MultiOutputRun> {
    let name = format!("multi_output_{count}");
    let plan = match side_by_side(count, output) {
        Ok(plan) => plan,
        Err(error) => {
            step(&name, false, &error);
            return None;
        }
    };
    let launcher_config = LauncherConfig {
        binary: config.resolve_session_launcher_binary()?,
        pam_service: config.pam_service.clone(),
        display: config.session_display.clone(),
        gpu_head: config.session_gpu_head.clone(),
        xorg_binary: config.xorg_bin.clone(),
        xorg_config_template: config.xorg_config_template.clone(),
        runtime_root: config.session_runtime_root.clone(),
        agent_binary: config.resolve_session_agent_binary()?,
        desktop: config.desktop_session.clone(),
        deskside: crate::deskside::LinuxDesksideConfig::default(),
        provider: DesktopProvider::Wayland,
        audio: crate::session::agent::AgentAudioArguments {
            local_playback: config.local_playback,
            pactl: config.pactl_bin.clone(),
        },
        // The temporary qualification account never sees a first-login
        // assistant over the desktop being measured.
        first_login_setup: arcen_session::desktop_session::first_login::FirstLoginSetup::Skip,
        // X11 applications are proven by the first output's own session.
        x11_apps: false,
    };
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
        .ok()?;
    let mut pending = match AuthenticatedLauncher::authenticate(
        &launcher_config,
        &user.name,
        zeroize::Zeroizing::new(user.password.as_str().to_owned()),
        "127.0.0.1",
        permit,
        crate::eventlog::random_correlation_id(),
    )
    .await
    {
        Ok(pending) => pending,
        Err(error) => {
            step(&name, false, &format!("authenticate: {error}"));
            return None;
        }
    };
    pending.set_multi_monitor_plan(Some(&plan));
    let started = Instant::now();
    let launcher = match pending.open().await {
        Ok(launcher) => launcher,
        Err(error) => {
            step(
                &name,
                false,
                &format!("headless session did not start: {error}"),
            );
            return None;
        }
    };
    let session_id = launcher
        .environment
        .session_id()
        .unwrap_or_default()
        .to_owned();
    let result = match launcher.wayland.clone() {
        Some(ready) if ready.streams.len() == count => {
            step(
                &name,
                true,
                &format!(
                    "{count} outputs {}x{}@{} ready in {} ms: {:?}",
                    output.0,
                    output.1,
                    output.2,
                    started.elapsed().as_millis(),
                    ready
                        .streams
                        .iter()
                        .map(|stream| (stream.connector.as_str(), stream.x, stream.node_id))
                        .collect::<Vec<_>>()
                ),
            );
            capture_concurrently(
                config,
                &launcher,
                &ready,
                &name,
                gpu_import_verified,
                native_encoder,
            )
            .await
        }
        Some(ready) => {
            step(
                &name,
                false,
                &format!("{} streams for {count} outputs", ready.streams.len()),
            );
            None
        }
        None => {
            step(&name, false, "the launcher started no Wayland session");
            None
        }
    };
    let runtime_dir = launcher
        .wayland
        .as_ref()
        .map(|ready| ready.runtime_dir.clone());
    let stopped = launcher.shutdown().await;
    let gone = match runtime_dir {
        Some(runtime_dir) => session_gone(&session_id, &runtime_dir).await,
        None => true,
    };
    step(
        &format!("{name}_teardown"),
        stopped && gone,
        &format!("agent stopped={stopped}; logind session {session_id} gone={gone}"),
    );
    result.filter(|_| stopped && gone)
}

async fn capture_concurrently(
    config: &crate::cli::Config,
    launcher: &crate::session::launcher::SessionLauncher,
    ready: &super::WaylandAgentReady,
    name: &str,
    gpu_import_verified: bool,
    native_encoder: CapabilityFact,
) -> Option<MultiOutputRun> {
    let escapes = leave_overview(ready).await;
    if let Err(error) = escapes {
        step(name, false, &format!("leave overview: {error}"));
    }
    let run = Run {
        config,
        launcher,
        ready,
        gpu_import: None,
        // As the single-output runs found it: software on a host without
        // a native encoder.
        native_encoder,
    };
    let template = match run.capenc_config(false) {
        Ok(template) => template,
        Err(error) => {
            step(name, false, &error);
            return None;
        }
    };
    let mut sessions = Vec::with_capacity(ready.streams.len());
    for stream in &ready.streams {
        let mut config = template.clone();
        let handoff = match ready.stream_handoff(stream, ready.generation) {
            Ok(handoff) => handoff,
            Err(error) => {
                step(
                    name,
                    false,
                    &format!("{}: hand-off: {error}", stream.connector),
                );
                break;
            }
        };
        if let Some(execution) = config.execution.as_mut() {
            execution.environment = execution
                .environment
                .clone()
                .with_wayland_session(ready, &handoff);
        }
        config.width = stream.width;
        config.height = stream.height;
        // The route the product takes: GPU import once the single-output
        // run verified it on this host.
        config.gpu_import_verified = gpu_import_verified;
        match capenc::spawn(config).await {
            Ok((session, _plan)) => sessions.push(session),
            Err(error) => {
                step(
                    name,
                    false,
                    &format!("{}: capenc: {error}", stream.connector),
                );
                break;
            }
        }
    }
    if sessions.len() != ready.streams.len() {
        for session in sessions {
            session.shutdown().await;
        }
        return None;
    }
    let route = sessions
        .iter()
        .filter_map(|session| session.capture_admission().map(|notice| notice.copy_path))
        .map(|copy_path| match copy_path {
            CopyPath::GpuConvert => Some(CaptureRoute::GpuConvert),
            CopyPath::ZeroCopyImport => Some(CaptureRoute::ZeroCopyImport),
            CopyPath::HostCopy => Some(CaptureRoute::HostCopy),
            CopyPath::CrossGpu => None,
        })
        .collect::<Vec<_>>();
    let window = SECONDS_PER_OUTPUT * u32::try_from(ready.streams.len()).unwrap_or(1) + 1;
    let mut drains = Vec::new();
    let mut measurements = Vec::new();
    for session in &mut sessions {
        let mut frames = session.take_frames();
        drains.push(tokio::spawn(async move {
            let mut count = 0_u64;
            if let Some(frames) = frames.as_mut() {
                while frames.recv().await.is_some() {
                    count += 1;
                }
            }
            count
        }));
        measurements.push(tokio::spawn(measure(session.pipeline_stats(), window)));
    }
    // The pointer visits every output in turn, long enough for each output's
    // own rate to be held over consecutive seconds: a still output sends
    // only its keepalive, and only the output under the pointer repaints.
    let mut accepted = 0;
    for stream in &ready.streams {
        let mut visiting = ready.clone();
        visiting.streams = vec![stream.clone()];
        let motion = MotionDriver::start(&visiting);
        tokio::time::sleep(Duration::from_secs(u64::from(SECONDS_PER_OUTPUT))).await;
        accepted += motion.finish().await.unwrap_or(0);
    }
    let mut outputs = Vec::with_capacity(sessions.len());
    let mut detail = Vec::new();
    for (stream, measurement) in ready.streams.iter().zip(measurements) {
        let (source, encoded, samples) = measurement.await.unwrap_or((None, None, Vec::new()));
        detail.push(format!(
            "{} {}x{}: (source, encoded) {samples:?} sustained source {source:?} encoded {encoded:?}",
            stream.connector, stream.width, stream.height
        ));
        outputs.push(OutputRateMeasurement {
            width: stream.width,
            height: stream.height,
            refresh_hz: ready
                .outputs
                .iter()
                .find(|output| output.connector == stream.connector)
                .map_or(0, |output| output.refresh_hz),
            sustained_source_fps: source,
            sustained_encoded_fps: encoded,
        });
    }
    for session in sessions {
        session.shutdown().await;
    }
    let mut access_units = 0;
    for drain in drains {
        access_units += drain.await.unwrap_or(0);
    }
    let run = MultiOutputRun {
        outputs,
        route: route
            .first()
            .copied()
            .flatten()
            .filter(|first| route.iter().all(|route| *route == Some(*first)))
            .unwrap_or(CaptureRoute::HostCopy),
        codec: template.codec.clone(),
        seconds: window,
    };
    step(
        name,
        run.every_output_encoded(),
        &format!(
            "route {}; {access_units} access units; {accepted} pointer motions; {}",
            run.route.token(),
            detail.join("; ")
        ),
    );
    Some(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_qualification_layout_is_a_row_with_the_first_output_primary() {
        let plan = side_by_side(4, (1920, 1080, 60)).unwrap();
        assert_eq!(plan.monitors.len(), 4);
        assert_eq!((plan.virtual_width, plan.virtual_height), (7680, 1080));
        assert!(plan.monitors[0].primary);
        assert_eq!(
            plan.monitors
                .iter()
                .map(|monitor| monitor.x)
                .collect::<Vec<_>>(),
            vec![0, 1920, 3840, 5760]
        );
    }
}
