//! Lab harness for the headless GNOME/Mutter Wayland session adapter.
//!
//! Run as root on a Linux lab host, never on a production Pier:
//!
//! ```text
//! wayland_session_harness --user <name> --password-file <file> \
//!     --launcher <wrapper> --pier <arcen-pier> --client <gtk-client.py> \
//!     [--capenc <arcen-capenc>] [--kill-compositor] [--dismiss escape|dbus|click] \
//!     [--shift-warmup] [--settle-ms <ms>] [--screenshots <dir>] [--xkb-layout <xkb>]
//!     [--x11 [--x11-client] [--x11-glx] [--x11-gears] [--x11-pointer] [--x11-probe <wayland-x11-probe.py>]]
//!     [--first-login-setup run|skip]
//!     [--pipeline auto|speed --capenc-run <capenc> [--codec h264|h265] [--fps N] [--encoder auto|nvenc|software-h264]
//!      [--moving-seconds N] [--idle-seconds N] [--stream-out <file>]]
//! ```
//!
//! With `--pipeline`, the first-frame proof is the product path instead of
//! `gst-launch-1.0`: the harness starts capenc through the Pier's own
//! `media::capenc::spawn` with the session's environment (the capture
//! hand-off), so the Wayland Auto or Speed composition admits the source,
//! states `ADMISSION`, and streams framed access units into the same
//! receiver the Pier forwards to a Deck. `--capenc-run` is a standalone
//! capenc built with `--features wayland`, or a wrapper that execs
//! `arcen-pier capenc "$@"`; it must be executable by the session user. The
//! run has a moving phase (the test client animates), an idle phase (the
//! client stops; GNOME 40 then sends no frames), and a keyframe requested
//! while idle. This is a lab path: it never makes Wayland selectable.
//!
//! `--launcher` must exec `arcen-pier session-launcher "$@"`; `--pier` is the
//! fused `arcen-pier` binary built with `--features wayland-provider`. The
//! harness authenticates through PAM exactly as the Pier does, opens a
//! headless Wayland session through the privileged launcher, proves a real
//! frame with `gst-launch-1.0 pipewiresrc`, optionally runs
//! `arcen-capenc wayland-capture-probe` (built with `--features wayland`) on
//! the session's real capture hand-off,
//! injects keyboard, pointer and scroll through the Pier's own session-bound
//! injector (`input::wayland`) into a GTK test client — including the first
//! key after the desktop became ready, held-input release on lane loss,
//! injector drop, supersession and disconnect, and the session's refusals —
//! optionally kills the compositor, tears the session down and checks that
//! nothing is left.
//!
//! With `--x11` the session runs the compositor's Xwayland: the harness
//! reports the agent's X11 proof and checks that no X11 socket or lock file
//! of the session is left after teardown (also after `--kill-compositor`).
//! `--x11-client` runs the same GTK test client on its X11 backend through
//! that Xwayland, so every input assertion is made against an X11 window;
//! `--x11-glx` runs `glxinfo -B` in the session; `--x11-gears` runs
//! `glxgears -fullscreen` (mesa-demos) for eleven seconds and reports its
//! frame rate; `--x11-probe` copies text
//! from an X11 client and pastes it in a Wayland client, then the reverse,
//! through the compositor's selection bridge, and compares the frame rate a
//! GTK client reaches on its X11 and Wayland backends. Run the
//! core-protocol `scripts/lab/x11-input-client.py` as `--client` with
//! `--x11-client` to assert input against a toolkit-free X11 window.

#[cfg(target_os = "linux")]
mod harness {
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::Duration;

    use arcen_media::video::{CaptureMemory, CopyPath, MotionPriority, PipelineId};
    use arcen_media::{
        BitDepth, ColorMatrix, ColorPrimaries, ColorRange, EncodeIntent, TransferCharacteristics,
    };
    use arcen_pier_linux::input::keymap::MOD_SHIFT;
    use arcen_pier_linux::input::wayland::{
        InjectorLoss, InjectorStatus, WaylandInjector, WaylandInputError, WaylandInputTarget,
    };
    use arcen_pier_linux::input::PierInput;
    use arcen_pier_linux::media::capenc::{self, CapencConfig, EncoderSelection};
    use arcen_pier_linux::session::launcher::SessionLauncher;
    use arcen_pier_linux::session::launcher::{AuthenticatedLauncher, LauncherConfig};
    use arcen_pier_linux::session::wayland::WaylandAgentReady;
    use arcen_pier_linux::session::wayland::{InputCommand, InputReply, InputRequest};
    use arcen_protocol::messages::{CursorMode, VideoSelectionIntent};
    use arcen_protocol::messages::{
        KeyEventMsg, MouseButtonMsg, MouseMoveMsg, MouseMoveRelativeMsg, MouseScrollMsg,
        ScrollUnitMsg,
    };
    use arcen_session::desktop_provider::CapabilityFact;
    use arcen_session::desktop_provider::DesktopProvider;
    use arcen_session::desktop_session::first_login::FirstLoginSetup;
    use arcen_session::wayland_session::{
        FrameEvidence, FrameImport, FrameMemory, HeadlessCompositor, RemoteDesktopEvidence,
        ScreenCastStream, VerifiedOutput, WaylandSessionReadiness,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const QT_ESCAPE: u32 = 0x0100_0000;
    const QT_SHIFT: u32 = 0x0100_0020;

    fn flag(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|argument| argument == name)
            .and_then(|index| args.get(index + 1))
            .cloned()
    }

    fn required(args: &[String], name: &str) -> String {
        flag(args, name).unwrap_or_else(|| panic!("missing {name}"))
    }

    async fn run_text(program: &str, args: &[&str]) -> String {
        let output = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .expect("run command");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// Processes in a session scope of `uid`: (session id, pid, process uid).
    fn scope_processes(uid: u32, comm: &str) -> Vec<(String, u32, u32)> {
        let slice = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&slice).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(session) = name
                .strip_prefix("session-")
                .and_then(|rest| rest.strip_suffix(".scope"))
            else {
                continue;
            };
            let procs =
                std::fs::read_to_string(entry.path().join("cgroup.procs")).unwrap_or_default();
            for pid in procs
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
            {
                let matches = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|text| text.trim() == comm);
                let owner = std::fs::read_to_string(format!("/proc/{pid}/status"))
                    .ok()
                    .and_then(|status| {
                        status
                            .lines()
                            .find_map(|line| line.strip_prefix("Uid:"))
                            .and_then(|uids| uids.split_whitespace().next()?.parse().ok())
                    });
                if let (true, Some(owner)) = (matches, owner) {
                    found.push((session.to_owned(), pid, owner));
                }
            }
        }
        found
    }

    /// Waits for the starting session agent (the session user's
    /// `arcen-pier` in a session scope) to have its compositor up — the
    /// private `wayland-0` socket exists — and stops the agent with SIGSTOP,
    /// so it stalls between compositor start and output verification.
    async fn freeze_starting_agent(uid: u32) -> Option<(String, u32)> {
        for _ in 0..800 {
            if let Some((session, pid, _)) =
                scope_processes(uid, "arcen-pier")
                    .into_iter()
                    .find(|(session, _, owner)| {
                        *owner == uid
                            && std::path::Path::new(&format!(
                                "/run/user/{uid}/arcen-wayland-{session}/wayland-0"
                            ))
                            .exists()
                    })
            {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(i32::try_from(pid).ok()?),
                    nix::sys::signal::Signal::SIGSTOP,
                );
                return Some((session, pid));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        None
    }

    fn report(step: &str, ok: bool, detail: &str) {
        println!(
            "HARNESS {step}: {} {detail}",
            if ok { "PASS" } else { "FAIL" }
        );
    }

    /// Longer than the old 20 s open timeout, well inside the Wayland budget.
    const SLOW_START_FREEZE: Duration = Duration::from_secs(25);

    /// Stops the launcher (as the Pier's process-group SIGTERM would) while
    /// its agent is still starting, and checks the release ran anyway.
    async fn stop_during_start(pending: AuthenticatedLauncher, uid: u32) -> bool {
        let closed_line = format!("session closed for user {}", pending.identity.username);
        let stopper = tokio::spawn(async move {
            let (session, agent) = freeze_starting_agent(uid).await?;
            // Let the compositor's session services mount the document
            // portal's FUSE file system inside the runtime directory, so the
            // root-side release has a user FUSE mount to remove.
            let doc = format!("/run/user/{uid}/arcen-wayland-{session}/doc ");
            let mut doc_mounted = false;
            for _ in 0..50 {
                if std::fs::read_to_string("/proc/mounts")
                    .unwrap_or_default()
                    .contains(&doc)
                {
                    doc_mounted = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let (_, launcher, _) = scope_processes(uid, "arcen-pier")
                .into_iter()
                .find(|(scope, _, owner)| *scope == session && *owner == 0)?;
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(i32::try_from(launcher).ok()?),
                nix::sys::signal::Signal::SIGTERM,
            );
            Some((session, agent, launcher, doc_mounted))
        });
        let started = std::time::Instant::now();
        let opened = pending.open().await;
        let stopped = stopper.await.ok().flatten();
        let open_failed = opened.is_err();
        let open_ms = started.elapsed().as_millis();
        let Some((session, agent, launcher, doc_mounted)) = stopped else {
            report(
                "stop_during_start",
                false,
                "the starting agent was not found",
            );
            return false;
        };
        let scope = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/session-{session}.scope");
        let runtime_dir = PathBuf::from(format!("/run/user/{uid}/arcen-wayland-{session}"));
        let mut released = false;
        for _ in 0..80 {
            let listed = run_text("/usr/bin/loginctl", &["list-sessions", "--no-legend"])
                .await
                .lines()
                .any(|line| line.split_whitespace().next() == Some(session.as_str()));
            let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
            if !listed
                && !std::path::Path::new(&scope).exists()
                && std::fs::symlink_metadata(&runtime_dir).is_err()
                && !mounts.contains(&runtime_dir.to_string_lossy().into_owned())
            {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let pam_closed = run_text(
            "/usr/bin/journalctl",
            &[
                "--since",
                "-2min",
                "--no-pager",
                "-o",
                "cat",
                "-t",
                "arcen-pier",
            ],
        )
        .await
        .lines()
        .any(|line| line.contains(&closed_line));
        let ok = open_failed && released && pam_closed;
        report(
            "stop_during_start",
            ok,
            &format!(
                "agent pid {agent} frozen mid-start, launcher pid {launcher} sent SIGTERM (user FUSE mount doc/ present at stop={doc_mounted}); open failed={open_failed} after {open_ms} ms; logind session {session} ended, scope and runtime dir {} gone, no mounts={released}; PAM session closed={pam_closed}",
                runtime_dir.display()
            ),
        );
        let mut manager_stopped = false;
        for _ in 0..60 {
            if run_text("/usr/bin/pgrep", &["-u", &uid.to_string()])
                .await
                .trim()
                .is_empty()
            {
                manager_stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        report(
            "user_manager",
            manager_stopped,
            &format!("no process of uid {uid} remains={manager_stopped}"),
        );
        ok && manager_stopped
    }

    /// First real frame, consumed as the session user through the
    /// session's private PipeWire with `gst-launch-1.0`.
    async fn gst_first_frame(
        launcher: &SessionLauncher,
        wayland: &WaylandAgentReady,
        readiness: &mut WaylandSessionReadiness,
    ) -> bool {
        // First real frame, consumed as the session user through the
        // session's private PipeWire, exactly where capenc would connect.
        // GNOME 40 ScreenCast produces frames on damage only: an idle
        // desktop delivers none, so the probe runs while the test client
        // maps and draws.
        let stream = &wayland.streams[0];
        let mut gst = tokio::process::Command::new("/usr/bin/gst-launch-1.0");
        gst.args([
            "-v",
            "pipewiresrc",
            &format!("path={}", stream.node_id),
            "num-buffers=3",
            "!",
            // Sized like capenc: an unconstrained consumer of a remote
            // monitor fixes Mutter's default 1280x720 and resizes it.
            &format!(
                "video/x-raw,width={},height={}",
                stream.width, stream.height
            ),
            "!",
            "fakesink",
            "sync=false",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        launcher.execution().configure(&mut gst).expect("user gst");
        let frame = tokio::time::timeout(Duration::from_secs(20), gst.output()).await;
        let frame_text = match frame {
            Ok(Ok(output)) => format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            other => format!("{other:?}"),
        };
        let caps = frame_text
            .lines()
            .find(|line| line.contains("fakesink") && line.contains("caps = video/x-raw"))
            .unwrap_or_default()
            .to_owned();
        let field = |name: &str| {
            caps.split(&format!("{name}=(")).nth(1).and_then(|rest| {
                rest.split_once(')')
                    .map(|(_, value)| value.split([',', ' ']).next().unwrap_or_default())
            })
        };
        let format = field("format").unwrap_or_default().to_owned();
        let width = field("width")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let height = field("height")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let eos = frame_text.contains("Got EOS from element");
        println!("HARNESS frame caps {caps}");
        let frame_result = readiness.record_frame(FrameEvidence {
            node_id: stream.node_id,
            format: format.clone(),
            width,
            height,
            memory: FrameMemory::Unknown,
            import: FrameImport::Unreported,
        });
        let frame_ok = eos && frame_result.is_ok();
        report(
            "first_frame",
            frame_ok,
            &format!(
                "node={} format={format} size={width}x{height} buffers=3 eos={eos} readiness={frame_result:?}",
                stream.node_id
            ),
        );
        if !frame_ok {
            println!("HARNESS gst output: {frame_text}");
        }
        frame_ok
    }

    /// GNOME Shell 40 starts in the Activities overview; leave it through
    /// the shell's own property on the session's private bus.
    async fn dismiss_overview(launcher: &SessionLauncher, compositor: &str) {
        if compositor != "gnome-shell" {
            return;
        }
        for _ in 0..20 {
            let mut set = tokio::process::Command::new("/usr/bin/gdbus");
            set.args(["call", "--session", "--dest", "org.gnome.Shell"])
                .args(["--object-path", "/org/gnome/Shell", "--method"])
                .args([
                    "org.freedesktop.DBus.Properties.Set",
                    "org.gnome.Shell",
                    "OverviewActive",
                    "<false>",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            launcher
                .execution()
                .configure(&mut set)
                .expect("user gdbus");
            let _ = set.output().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut get = tokio::process::Command::new("/usr/bin/gdbus");
            get.args(["call", "--session", "--dest", "org.gnome.Shell"])
                .args(["--object-path", "/org/gnome/Shell", "--method"])
                .args([
                    "org.freedesktop.DBus.Properties.Get",
                    "org.gnome.Shell",
                    "OverviewActive",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            launcher
                .execution()
                .configure(&mut get)
                .expect("user gdbus");
            let state = get
                .output()
                .await
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default();
            if state.contains("false") {
                println!("HARNESS overview dismissed: {}", state.trim());
                return;
            }
        }
        println!("HARNESS overview still active");
    }

    #[derive(Default)]
    struct Phase {
        access_units: u32,
        keyframes: u32,
        bytes: u64,
    }

    async fn drain(
        frames: &mut tokio::sync::mpsc::Receiver<arcen_pier_linux::media::annexb::AccessUnit>,
        duration: Duration,
        out: &mut Option<std::fs::File>,
    ) -> Phase {
        use std::io::Write as _;
        let mut phase = Phase::default();
        let deadline = tokio::time::Instant::now() + duration;
        while let Ok(Some(unit)) = tokio::time::timeout_at(deadline, frames.recv()).await {
            phase.access_units += 1;
            phase.keyframes += u32::from(unit.is_keyframe);
            phase.bytes += unit.data.len() as u64;
            if let Some(file) = out.as_mut() {
                let _ = file.write_all(&unit.data);
            }
        }
        phase
    }

    /// The product capture path on this session: capenc through the Pier's
    /// own supervision, the session's hand-off, one Wayland composition.
    #[allow(clippy::too_many_lines)] // One linear lab scenario.
    async fn run_pipeline(
        args: &[String],
        preset: &str,
        launcher: &SessionLauncher,
        wayland: &WaylandAgentReady,
        readiness: &mut WaylandSessionReadiness,
        client_pid: Option<u32>,
    ) -> bool {
        let (default_fps, priority, requested) = match preset {
            "auto" => (30, MotionPriority::Detail, PipelineId::Auto),
            "speed" => (60, MotionPriority::Motion, PipelineId::Speed),
            other => panic!("--pipeline must be auto or speed, not {other}"),
        };
        let fps = flag(args, "--fps").map_or(default_fps, |value| value.parse().expect("--fps"));
        let seconds = |name: &str, default: u64| {
            Duration::from_secs(
                flag(args, name)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(default),
            )
        };
        let stream = &wayland.streams[0];
        let config = CapencConfig {
            binary: PathBuf::from(required(args, "--capenc-run")),
            output_index: 0,
            codec: flag(args, "--codec").unwrap_or_else(|| "h264".into()),
            // `--encoder auto` runs the Pier's own ladder (native, then
            // software); `software-h264` the Wayland software composition.
            encoder: match flag(args, "--encoder").as_deref() {
                None | Some("nvenc") => EncoderSelection::NativeNvenc,
                Some("auto") => EncoderSelection::Auto,
                Some("software-h264") => EncoderSelection::SoftwareH264,
                Some(other) => {
                    panic!("--encoder must be auto, nvenc or software-h264, not {other}")
                }
            },
            fps,
            yuv444: false,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            transfer: TransferCharacteristics::Bt709,
            color_primaries: ColorPrimaries::Bt709,
            desktop_encoding: arcen_media::video::DesktopSignalEncoding::Sdr,
            video_selection: VideoSelectionIntent::AdaptivePerformance,
            codec_pinned: false,
            variant_pinned: false,
            intent: EncodeIntent::Interactive,
            motion_priority: priority,
            qp_map: arcen_media::video::QpMapPolicy::Off,
            debug_diagnostics: true,
            encoder_max_bitrate_bps: arcen_media::video::pipeline_contract(requested)
                .encoder_ceiling_bps(),
            width: stream.width,
            height: stream.height,
            // The Deck's usual request; the session's stream decides.
            cursor_mode: CursorMode::Local,
            capture_rate: None,
            capture_rate_evidence: arcen_media::video::CaptureRateEvidence::Measured,
            capture_rates_by_codec: Vec::new(),
            speed_runtime_shortfall: false,
            gpu_import_verified: false,
            wayland_fidelity_args: Vec::new(),
            display: None,
            xauthority: None,
            execution: Some(launcher.execution()),
            session_log_id: arcen_telemetry::CorrelationId::parse_uuid(
                "7a2d3c4e-5f60-4718-8a9b-0c1d2e3f4a5b",
            )
            .expect("log id"),
        };
        let started = std::time::Instant::now();
        let (mut session, plan) = match capenc::spawn(config).await {
            Ok(spawned) => spawned,
            Err(error) => {
                report("capenc_pipeline", false, &format!("spawn failed: {error}"));
                return false;
            }
        };
        let ready_ms = started.elapsed().as_millis();
        let Some(notice) = session.capture_admission().copied() else {
            report(
                "capenc_pipeline",
                false,
                "READY without a capture admission",
            );
            session.shutdown().await;
            return false;
        };
        let served = notice.served().copied();
        println!(
            "HARNESS pipeline admission requested={} decision={} served={:?} fps={:?} \
             fps_limited_by={:?} copy_path={} colour_assumed={:?} cursor={} cursor_mode={:?} \
             degradation={:?}",
            notice.requested.pipeline().token(),
            notice.admission.decision_token(),
            served.map(|served| served.pipeline.token()),
            served.map(|served| served.fps_ceiling),
            served.and_then(|served| served.fps_limited_by),
            notice.copy_path.token(),
            served.map(|served| served.colour_assumed),
            notice.cursor.token(),
            served.and_then(|served| served.cursor_mode),
            notice.degradation(),
        );
        println!(
            "HARNESS pipeline plan backend={} codec={} chroma={} depth={} range={} matrix={} \
             {}x{}@{} cursor={:?} ready_ms={ready_ms}",
            plan.backend.ready_token(),
            plan.codec_token(),
            plan.chroma_token(),
            plan.bit_depth_token(),
            plan.range_token(),
            plan.matrix_token(),
            plan.width,
            plan.height,
            plan.fps,
            plan.cursor_mode,
        );
        let frame_result = readiness.record_frame(FrameEvidence {
            node_id: notice.frame.node_id,
            format: notice.frame.format.token().to_owned(),
            width: notice.frame.width,
            height: notice.frame.height,
            memory: match notice.frame.memory {
                CaptureMemory::HostShared => FrameMemory::SharedMemory,
                CaptureMemory::DmaBuf { .. } => FrameMemory::DmaBuf,
                CaptureMemory::GpuDevice => FrameMemory::Unknown,
            },
            import: match notice.copy_path {
                CopyPath::HostCopy => FrameImport::HostCopy,
                CopyPath::GpuConvert => FrameImport::GpuConvert,
                CopyPath::ZeroCopyImport => FrameImport::ZeroCopyImport,
                CopyPath::CrossGpu => FrameImport::Unreported,
            },
        });
        report(
            "first_frame",
            frame_result.is_ok(),
            &format!(
                "capenc node={} format={} size={}x{} memory={} import={} readiness={frame_result:?}",
                notice.frame.node_id,
                notice.frame.format.token(),
                notice.frame.width,
                notice.frame.height,
                notice.frame.memory.token(),
                notice.copy_path.token(),
            ),
        );
        let mut frames = session.take_frames().expect("frames");
        let mut out =
            flag(args, "--stream-out").map(|path| std::fs::File::create(path).expect("stream out"));
        let moving_for = seconds("--moving-seconds", 10);
        let moving = drain(&mut frames, moving_for, &mut out).await;
        // Stop the client's animation: GNOME 40 then sends no frames.
        if let Some(pid) = client_pid {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(-1)),
                nix::sys::signal::Signal::SIGUSR1,
            );
        }
        let _settle = drain(&mut frames, Duration::from_millis(1500), &mut out).await;
        let idle_for = seconds("--idle-seconds", 10);
        let idle = drain(&mut frames, idle_for, &mut out).await;
        // A Deck's recovery request on a still desktop.
        let requested_at = tokio::time::Instant::now();
        let idr_sent = session.idr().request();
        let mut keyframe_ms = None;
        while let Ok(Some(unit)) =
            tokio::time::timeout_at(requested_at + Duration::from_secs(2), frames.recv()).await
        {
            if let Some(file) = out.as_mut() {
                use std::io::Write as _;
                let _ = file.write_all(&unit.data);
            }
            if unit.is_keyframe {
                keyframe_ms = Some(requested_at.elapsed().as_millis());
                break;
            }
        }
        session.shutdown().await;
        let moving_fps = f64::from(moving.access_units) / moving_for.as_secs_f64();
        let moving_kbps = moving.bytes as f64 * 8.0 / 1000.0 / moving_for.as_secs_f64();
        let idle_aus_per_s = f64::from(idle.access_units) / idle_for.as_secs_f64();
        let expected_fps = f64::from(plan.fps);
        // Moving: the served cadence (the client redraws faster than it).
        // Idle: Keel's keepalive only, about one access unit a second.
        let ok = frame_result.is_ok()
            && moving_fps >= expected_fps * 0.5
            && moving_fps <= expected_fps * 1.1
            && idle_aus_per_s <= 2.0
            && idle.keyframes == 0
            && idr_sent
            && keyframe_ms.is_some();
        report(
            "capenc_pipeline",
            ok,
            &format!(
                "served={:?} moving: {} AUs in {:?} = {moving_fps:.1} fps {moving_kbps:.0} kbps \
                 keyframes={}; idle: {} AUs in {:?} = {idle_aus_per_s:.2}/s keyframes={}; \
                 keyframe on demand while idle after {keyframe_ms:?} ms",
                served.map(|served| served.pipeline.token()),
                moving.access_units,
                moving_for,
                moving.keyframes,
                idle.access_units,
                idle_for,
                idle.keyframes,
            ),
        );
        ok
    }

    #[allow(clippy::too_many_lines)] // One linear lab scenario.
    pub async fn main() -> bool {
        let args = std::env::args().collect::<Vec<_>>();
        let user = required(&args, "--user");
        let password = std::fs::read_to_string(required(&args, "--password-file"))
            .expect("password file")
            .trim()
            .to_owned();
        let pier = PathBuf::from(required(&args, "--pier"));
        let client = required(&args, "--client");
        let kill_compositor = args.iter().any(|argument| argument == "--kill-compositor");
        let terminate_session = args
            .iter()
            .any(|argument| argument == "--terminate-session");
        let dismiss = flag(&args, "--dismiss").unwrap_or_else(|| "escape".to_owned());
        let screenshots = flag(&args, "--screenshots");
        let xkb_layout = flag(&args, "--xkb-layout");
        let settle_ms = flag(&args, "--settle-ms")
            .and_then(|value| value.parse().ok())
            .unwrap_or(1000_u64);
        let x11_client = args.iter().any(|argument| argument == "--x11-client");
        let x11_glx = args.iter().any(|argument| argument == "--x11-glx");
        let x11_gears = args.iter().any(|argument| argument == "--x11-gears");
        let x11_pointer = args.iter().any(|argument| argument == "--x11-pointer");
        let x11_clipboard = flag(&args, "--x11-probe");
        let x11 = x11_client
            || x11_glx
            || x11_gears
            || x11_pointer
            || x11_clipboard.is_some()
            || args.iter().any(|argument| argument == "--x11");
        let mut all = true;

        // What a Pier with `capture.provider = "wayland"` decides at start
        // on this host.
        let selection =
            arcen_pier_linux::session::wayland::select_configured_provider(
                arcen_session::desktop_provider::ProviderPreference::Wayland,
                std::path::Path::new("/nonexistent/Xorg"),
                std::path::Path::new(&flag(&args, "--record").unwrap_or_else(|| {
                    arcen_session::wayland_qualification::RECORD_PATH.to_owned()
                })),
            )
            .await;
        report(
            "provider_selection",
            true,
            &match selection {
                Ok(selected) => format!("selected {:?}", selected.selection.provider),
                Err(refused) => format!(
                    "refused, no Xorg fallback: blockers={:?}; {refused}",
                    refused.tokens()
                ),
            },
        );

        let config = LauncherConfig {
            binary: PathBuf::from(required(&args, "--launcher")),
            pam_service: "login".into(),
            display: ":99".into(),
            gpu_head: "DFP-0".into(),
            xorg_binary: "/nonexistent/Xorg".into(),
            xorg_config_template: "/nonexistent/xorg.conf".into(),
            runtime_root: "/nonexistent/runtime".into(),
            agent_binary: pier,
            desktop: "gnome".into(),
            deskside: Default::default(),
            provider: DesktopProvider::Wayland,
            x11_apps: x11,
            audio: Default::default(),
            first_login_setup: flag(&args, "--first-login-setup")
                .map_or(FirstLoginSetup::Run, |token| {
                    FirstLoginSetup::from_token(&token).expect("--first-login-setup run|skip")
                }),
        };
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .expect("permit");
        let session_log_id =
            arcen_telemetry::CorrelationId::parse_uuid("6f1c2b4e-7a0d-4c9e-9f31-2d8a5b7c0e14")
                .expect("log id");
        let pending = AuthenticatedLauncher::authenticate(
            &config,
            &user,
            zeroize::Zeroizing::new(password),
            "203.0.113.10",
            permit,
            session_log_id,
        )
        .await
        .expect("PAM authentication");
        report("authenticate", true, &format!("user={user}"));
        let uid = pending.identity.uid;
        if args
            .iter()
            .any(|argument| argument == "--stop-during-start")
        {
            return stop_during_start(pending, uid).await;
        }
        let slow_start = args.iter().any(|argument| argument == "--slow-start");
        let freezer = slow_start.then(|| {
            tokio::spawn(async move {
                let frozen = freeze_starting_agent(uid).await;
                tokio::time::sleep(SLOW_START_FREEZE).await;
                if let Some((_, pid)) = &frozen {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(i32::try_from(*pid).unwrap_or(-1)),
                        nix::sys::signal::Signal::SIGCONT,
                    );
                }
                frozen
            })
        });
        let started = std::time::Instant::now();
        let launcher = pending.open().await.expect("open headless session");
        if let Some(freezer) = freezer {
            let frozen = freezer.await.ok().flatten();
            let open_ms = started.elapsed().as_millis();
            let ok = frozen.is_some() && open_ms > 20_000;
            all &= ok;
            report(
                "slow_start",
                ok,
                &format!(
                    "agent {frozen:?} stopped for {}s during start; the open still succeeded after {open_ms} ms (the old 20 s broker timeout would have killed it)",
                    SLOW_START_FREEZE.as_secs()
                ),
            );
        }
        let wayland = launcher.wayland.clone().expect("wayland ready message");
        let session_id = launcher
            .environment
            .session_id()
            .unwrap_or_default()
            .to_owned();
        report(
            "session_start",
            true,
            &format!(
                "logind_session={session_id} open_ms={} ready={}",
                started.elapsed().as_millis(),
                serde_json::to_string(&wayland).unwrap_or_default()
            ),
        );
        for key in [
            "XDG_SESSION_TYPE",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "WAYLAND_DISPLAY",
            "PULSE_RUNTIME_PATH",
            "ARCEN_CAPTURE_PROVIDER",
            "ARCEN_PIPEWIRE_NODE",
            "ARCEN_CAPTURE_OUTPUT",
            "ARCEN_CAPTURE_OUTPUT_GENERATION",
            "ARCEN_CAPTURE_EXPECTED_SIZE",
            "ARCEN_CAPTURE_OUTPUT_POSITION",
            "ARCEN_CAPTURE_CURSOR_MODE",
            "ARCEN_CAPTURE_GPU_PCI",
            "ARCEN_COMPOSITOR_GPU_PCI",
            "ARCEN_COMPOSITOR_RENDER_NODE",
            "DISPLAY",
        ] {
            println!(
                "HARNESS env {key}={}",
                launcher.environment.get(key).unwrap_or("<unset>")
            );
        }
        let x11_ready = wayland.x11.clone();
        if x11 {
            let ready = x11_ready.as_ref().is_some_and(|x11| x11.ready);
            all &= ready;
            report(
                "x11_apps",
                ready,
                &format!(
                    "display={:?} setup_display={:?} glamor={:?} xauthority={:?} failure={:?}; Pier env DISPLAY={}",
                    x11_ready.as_ref().and_then(|x11| x11.display()),
                    x11_ready.as_ref().and_then(|x11| x11.setup_display()),
                    x11_ready.as_ref().map(|x11| x11.glamor.token()),
                    x11_ready.as_ref().and_then(|x11| x11.xauthority.clone()),
                    x11_ready.as_ref().and_then(|x11| x11.failure.clone()),
                    launcher.environment.get("DISPLAY").unwrap_or("<unset>"),
                ),
            );
        } else {
            let none = x11_ready.is_none();
            all &= none;
            report(
                "x11_apps",
                none,
                "not requested; the agent reported no X11 state",
            );
        }
        let x11_env = x11_ready
            .as_ref()
            .and_then(|x11| Some((x11.display()?, x11.xauthority.clone()?)));
        if x11_glx {
            let glx = match &x11_env {
                Some((display, xauthority)) => {
                    let mut glxinfo = tokio::process::Command::new("/usr/bin/glxinfo");
                    glxinfo
                        .arg("-B")
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    launcher
                        .execution()
                        .configure(&mut glxinfo)
                        .expect("user glxinfo");
                    glxinfo
                        .env("DISPLAY", display)
                        .env("XAUTHORITY", xauthority);
                    tokio::time::timeout(Duration::from_secs(20), glxinfo.output())
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                        .unwrap_or_default()
                }
                None => String::new(),
            };
            let lines = glx
                .lines()
                .filter(|line| {
                    line.contains("direct rendering")
                        || line.contains("OpenGL renderer")
                        || line.contains("OpenGL vendor")
                        || line.contains("OpenGL version")
                })
                .map(str::trim)
                .collect::<Vec<_>>();
            let direct = glx.contains("direct rendering: Yes");
            all &= direct;
            report("x11_glx", direct, &lines.join(" | "));
        }
        if x11_gears {
            let gears = match &x11_env {
                Some((display, xauthority)) => {
                    let mut gears = tokio::process::Command::new("/usr/bin/glxgears");
                    gears
                        .args(["-fullscreen", "-info"])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    launcher
                        .execution()
                        .configure(&mut gears)
                        .expect("user glxgears");
                    gears.env("DISPLAY", display).env("XAUTHORITY", xauthority);
                    match gears.spawn() {
                        Ok(mut child) => {
                            let stdout = child.stdout.take().expect("glxgears stdout");
                            let mut lines = BufReader::new(stdout).lines();
                            let mut kept = Vec::new();
                            let _ = tokio::time::timeout(Duration::from_secs(11), async {
                                while let Ok(Some(line)) = lines.next_line().await {
                                    if line.contains("frames in")
                                        || line.contains("GL_RENDERER")
                                        || line.contains("VisualID")
                                    {
                                        kept.push(line.trim().to_owned());
                                    }
                                }
                            })
                            .await;
                            let _ = child.kill().await;
                            kept
                        }
                        Err(error) => vec![format!("glxgears did not start: {error}")],
                    }
                }
                None => vec!["no X11 display".to_owned()],
            };
            let rendered = gears.iter().any(|line| line.contains("frames in"));
            all &= rendered;
            report("x11_gears", rendered, &gears.join(" | "));
        }
        if let (Some(probe), Some((display, xauthority))) = (&x11_clipboard, &x11_env) {
            // A Wayland client reads the selection only with keyboard focus,
            // which a window opened over the Activities overview does not get.
            dismiss_overview(&launcher, &wayland.compositor).await;
            let spawn = |backend: &str, arguments: &[&str]| {
                let mut command = tokio::process::Command::new("/usr/bin/python3");
                command
                    .arg(probe)
                    .args(arguments)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                launcher
                    .execution()
                    .configure(&mut command)
                    .expect("user clipboard probe");
                command
                    .env("GDK_BACKEND", backend)
                    .env("DISPLAY", display)
                    .env("XAUTHORITY", xauthority);
                command.spawn().expect("spawn clipboard probe")
            };
            let first_line = |child: &mut tokio::process::Child| {
                let stdout = child.stdout.take().expect("probe stdout");
                async move {
                    tokio::time::timeout(
                        Duration::from_secs(20),
                        BufReader::new(stdout).lines().next_line(),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .flatten()
                    .unwrap_or_default()
                }
            };
            let mut results = Vec::new();
            for (from, to, token) in [
                ("x11", "wayland", "arcen-x11-to-wayland-7f3a"),
                ("wayland", "x11", "arcen-wayland-to-x11-91c2"),
            ] {
                let mut setter = spawn(from, &["set", token]);
                let set = first_line(&mut setter).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                let mut getter = spawn(to, &["get"]);
                let got = first_line(&mut getter).await;
                let _ = getter.wait().await;
                let _ = setter.kill().await;
                let ok = set.starts_with("SET") && got == format!("GOT {token}");
                results.push((ok, format!("{from}→{to}: setter {set:?}, getter {got:?}")));
            }
            let ok = results.iter().all(|(ok, _)| *ok);
            all &= ok;
            report(
                "x11_clipboard",
                ok,
                &results
                    .iter()
                    .map(|(_, detail)| detail.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            );
            // Frame-clock rate of the same GTK client on each backend: an
            // informational comparison, not an assertion.
            let mut rates = Vec::new();
            for backend in ["x11", "wayland"] {
                let mut probe = spawn(backend, &["frames"]);
                let stdout = probe.stdout.take().expect("probe stdout");
                let mut lines = BufReader::new(stdout).lines();
                let mut seconds = Vec::new();
                let _ = tokio::time::timeout(Duration::from_secs(15), async {
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Some(count) = line.strip_prefix("FRAMES ") {
                            seconds.push(count.to_owned());
                        }
                    }
                })
                .await;
                let _ = probe.kill().await;
                rates.push(format!("{backend}: {}", seconds.join(",")));
            }
            report("x11_frames", true, &rates.join("; "));
        }
        let logind = run_text(
            "/usr/bin/loginctl",
            &[
                "show-session",
                &session_id,
                "-p",
                "Type",
                "-p",
                "Class",
                "-p",
                "Seat",
                "-p",
                "Service",
                "-p",
                "Active",
                "-p",
                "Leader",
            ],
        )
        .await;
        let seatless = logind.contains("Type=wayland")
            && logind.contains("Class=user")
            && logind.contains("Seat=\n");
        all &= seatless;
        report("logind_session", seatless, &logind.replace('\n', " "));

        // Readiness as the shared state machine sees it.
        let mut readiness = WaylandSessionReadiness::new();
        let compositor = if wayland.compositor == "gnome-shell" {
            HeadlessCompositor::GnomeShell
        } else {
            HeadlessCompositor::Mutter
        };
        readiness.record_compositor(compositor).expect("compositor");
        readiness
            .record_outputs(
                wayland
                    .outputs
                    .iter()
                    .map(|output| VerifiedOutput {
                        connector: output.connector.clone(),
                        width: output.width,
                        height: output.height,
                        refresh_hz: output.refresh_hz,
                    })
                    .collect(),
            )
            .expect("outputs");
        for stream in &wayland.streams {
            readiness
                .record_stream(ScreenCastStream {
                    connector: stream.connector.clone(),
                    node_id: stream.node_id,
                    width: stream.width,
                    height: stream.height,
                })
                .expect("stream");
        }
        readiness
            .record_input(RemoteDesktopEvidence {
                device_types: wayland.input_device_types,
                started: true,
                linked_to_screencast: true,
                absolute_pointer_accepted: true,
            })
            .expect("input");
        report(
            "virtual_output",
            true,
            &format!("outputs={:?}", wayland.outputs),
        );

        // Audio stays on the user's PulseAudio, independent of ScreenCast:
        // the session environment must reach a server with a monitor source.
        let mut pactl = tokio::process::Command::new("/usr/bin/pactl");
        pactl
            .args(["list", "short", "sources"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        launcher
            .execution()
            .configure(&mut pactl)
            .expect("user pactl");
        let sources = tokio::time::timeout(Duration::from_secs(10), pactl.output())
            .await
            .ok()
            .and_then(Result::ok)
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let monitor = sources
            .lines()
            .find(|line| line.contains(".monitor"))
            .map(str::to_owned);
        all &= monitor.is_some();
        report(
            "audio_monitor",
            monitor.is_some(),
            &format!(
                "PULSE_RUNTIME_PATH={} source={monitor:?}",
                launcher
                    .environment
                    .get("PULSE_RUNTIME_PATH")
                    .unwrap_or_default()
            ),
        );

        // Keyboard, pointer and scroll into a GTK client in the session.
        let mut gtk = tokio::process::Command::new("/usr/bin/python3");
        gtk.arg(&client)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        launcher
            .execution()
            .configure(&mut gtk)
            .expect("user client");
        match (&x11_env, x11_client) {
            (Some((display, xauthority)), true) => {
                gtk.env("GDK_BACKEND", "x11")
                    .env("DISPLAY", display)
                    .env("XAUTHORITY", xauthority);
            }
            _ => {
                gtk.env("GDK_BACKEND", "wayland");
            }
        }
        if std::env::var("ARCEN_HARNESS_WAYLAND_DEBUG").is_ok_and(|value| !value.is_empty()) {
            gtk.env("WAYLAND_DEBUG", "client");
        }
        if let Ok(period) = std::env::var("ARCEN_LAB_ANIMATE_MS") {
            gtk.env("ARCEN_LAB_ANIMATE_MS", period);
        }
        let mut gtk = gtk.spawn().expect("spawn client");
        let mut client_lines = BufReader::new(gtk.stdout.take().expect("client stdout")).lines();
        let mut client_log = Vec::new();
        let mapped = tokio::time::timeout(Duration::from_secs(20), async {
            while let Ok(Some(line)) = client_lines.next_line().await {
                let mapped = line.starts_with("MAPPED");
                client_log.push(line);
                if mapped {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        let backend_ok = !x11_client
            || client_log
                .iter()
                .any(|line| line.starts_with("MAPPED backend=:"));
        all &= backend_ok;
        report(
            "input_client",
            mapped && backend_ok,
            &client_log.join(" | "),
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if let Some(preset) = flag(&args, "--pipeline") {
            // Leaving the overview without a key would make the input
            // stage's first key a focus-entering key whose press the client
            // never sees; only `--dismiss dbus` asks for that.
            if dismiss == "dbus" {
                dismiss_overview(&launcher, &wayland.compositor).await;
            }
            all &= run_pipeline(
                &args,
                &preset,
                &launcher,
                &wayland,
                &mut readiness,
                gtk.id(),
            )
            .await;
        } else {
            all &= gst_first_frame(&launcher, &wayland, &mut readiness).await;
        }
        match readiness.ready() {
            Ok(ready) => report(
                "readiness",
                true,
                &format!(
                    "compositor={} streams={:?} frame={:?} facts={:?}",
                    ready.compositor.token(),
                    ready.streams,
                    ready.frame,
                    readiness.proven_facts(Default::default())
                ),
            ),
            Err(gaps) => {
                all = false;
                report("readiness", false, &format!("gaps={gaps:?}"));
            }
        }
        let stream = &wayland.streams[0];

        // capenc consuming this session's real hand-off: the same session
        // environment the Pier gives capenc, parsed by capenc with the
        // shared contract, frames from this session's node.
        if let Some(capenc) = flag(&args, "--capenc") {
            let mut probe = tokio::process::Command::new(&capenc);
            probe
                .args([
                    "wayland-capture-probe",
                    "frames=30",
                    "encode-frames=10",
                    "timeout-ms=5000",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            launcher
                .execution()
                .configure(&mut probe)
                .expect("user capenc");
            let output = tokio::time::timeout(Duration::from_secs(90), probe.output()).await;
            let text = match output {
                Ok(Ok(output)) => format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
                other => format!("{other:?}"),
            };
            let handoff_line = text
                .lines()
                .find(|line| line.contains("wayland-probe handoff"))
                .unwrap_or_default()
                .to_owned();
            let summary = text
                .lines()
                .find(|line| line.contains("wayland-probe summary"))
                .unwrap_or_default()
                .to_owned();
            let node_matches = handoff_line.contains(&format!("node={} ", stream.node_id));
            let frames = summary
                .split_whitespace()
                .find_map(|field| field.strip_prefix("frames="))
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            let host_copy = text.lines().any(|line| {
                line.contains("wayland-probe frame") && line.contains("copy_path=host-copy")
            });
            let ok = node_matches && frames > 0 && host_copy;
            all &= ok;
            println!("HARNESS capenc {handoff_line}");
            println!("HARNESS capenc {summary}");
            if !ok {
                println!("HARNESS capenc output: {text}");
            }
            report(
                "capenc_handoff",
                ok,
                &format!(
                    "capenc parsed node {} from this session's hand-off and captured {frames} frames (host_copy={host_copy})",
                    stream.node_id
                ),
            );
        }

        // Session input through the Pier's own injector, exactly as the
        // input lane drives it: root connects to the session's socket,
        // proves it, and sends Deck protocol messages.
        let probe_facts = arcen_pier_linux::session::wayland::facts::probe_host()
            .await
            .facts();
        let target =
            WaylandInputTarget::from_session(&wayland, uid, &session_id, launcher.agent_pid)
                .expect("input target");
        let (injector, _stats) = WaylandInjector::connect(target.clone())
            .await
            .expect("Pier injector proves the session");
        let proof = injector.proof().clone();
        report(
            "input_facts",
            proof.capabilities().keyboard == CapabilityFact::Available,
            &format!(
                "hello={:?} capabilities={:?} host_facts_upgraded={:?}",
                proof.facts(),
                proof.capabilities(),
                proof.proven_facts(probe_facts)
            ),
        );
        let mut input = PierInput::Wayland(injector);
        let step = Duration::from_millis(120);
        // Input is aimed at an output a Deck is watching. Mutter drops
        // absolute motion for a remote monitor whose stream nobody consumes
        // (`meta_screen_cast_virtual_stream_transform_position`), so the
        // input stages run with a consumer, as a Deck's capture would be.
        let consumer = {
            let mut gst = tokio::process::Command::new("/usr/bin/gst-launch-1.0");
            gst.args([
                "-q",
                "pipewiresrc",
                &format!("path={}", stream.node_id),
                "!",
                &format!(
                    "video/x-raw,width={},height={}",
                    stream.width, stream.height
                ),
                "!",
                "fakesink",
                "sync=false",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
            launcher.execution().configure(&mut gst).expect("user gst");
            gst.spawn().ok()
        };
        println!(
            "HARNESS stream consumer for input: {}",
            consumer.as_ref().map_or("not started", |_| "started")
        );

        let shot = |name: &'static str| {
            let source = wayland.runtime_dir.join(format!("{name}.png"));
            let destination = screenshots
                .as_ref()
                .map(|dir| PathBuf::from(dir).join(format!("{name}.png")));
            let node = stream.node_id;
            let size_caps = format!(
                "video/x-raw,width={},height={}",
                stream.width, stream.height
            );
            let mut gst = tokio::process::Command::new("/usr/bin/gst-launch-1.0");
            gst.args([
                "-q",
                "pipewiresrc",
                &format!("path={node}"),
                "num-buffers=1",
                "!",
                &size_caps,
                "!",
                "videoconvert",
                "!",
                "pngenc",
                "!",
                "filesink",
                &format!("location={}", source.display()),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
            launcher.execution().configure(&mut gst).expect("user gst");
            async move {
                let Some(destination) = destination else {
                    return;
                };
                let _ = tokio::time::timeout(Duration::from_secs(10), gst.status()).await;
                let _ = std::fs::copy(&source, &destination);
                let _ = std::fs::remove_file(&source);
                println!("HARNESS screenshot {}", destination.display());
            }
        };
        shot("before-dismissal").await;

        // GNOME Shell 40 opens in the Activities overview, and on a user's
        // first login (with gnome-tour installed) also opens its modal
        // welcome dialog; both take keyboard and pointer, exactly as on a
        // console. The Deck user dismisses them with Escape. `--dismiss
        // dbus` reproduces the earlier harness (OverviewActive=false).
        let overview = |value: &'static str| {
            let mut gdbus = tokio::process::Command::new("/usr/bin/gdbus");
            gdbus
                .args(["call", "--session", "--dest", "org.gnome.Shell"])
                .args(["--object-path", "/org/gnome/Shell", "--method"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            if value.is_empty() {
                gdbus.args([
                    "org.freedesktop.DBus.Properties.Get",
                    "org.gnome.Shell",
                    "OverviewActive",
                ]);
            } else {
                gdbus.args([
                    "org.freedesktop.DBus.Properties.Set",
                    "org.gnome.Shell",
                    "OverviewActive",
                    value,
                ]);
            }
            launcher
                .execution()
                .configure(&mut gdbus)
                .expect("user gdbus");
            async move {
                gdbus
                    .output()
                    .await
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                    .unwrap_or_default()
            }
        };
        let key = |scan_code: u32, pressed: bool, modifiers: u32| KeyEventMsg {
            scan_code,
            pressed,
            modifiers,
            ..KeyEventMsg::default()
        };
        let mut overview_state = String::new();
        let mut escapes = 0;
        if wayland.compositor == "gnome-shell" {
            for _ in 0..6 {
                overview_state = overview("").await;
                if overview_state.contains("false") {
                    break;
                }
                if dismiss == "dbus" {
                    overview("<false>").await;
                } else if dismiss == "click" {
                    // The way a Deck user leaves the overview: click the
                    // client's window, which the overview shows centred.
                    for pressed in [true, false] {
                        input
                            .mouse_button(&MouseButtonMsg {
                                msg_type: "mouse_button".into(),
                                x: 0.5,
                                y: 0.5,
                                button: 1,
                                pressed,
                                ..MouseButtonMsg::default()
                            })
                            .expect("click");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                } else {
                    escapes += 1;
                    input.key_event(&key(QT_ESCAPE, true, 0)).expect("escape");
                    input.key_event(&key(QT_ESCAPE, false, 0)).expect("escape");
                }
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
        }
        report(
            "desktop_ready",
            overview_state.contains("false") || wayland.compositor != "gnome-shell",
            &format!("dismiss={dismiss} escapes={escapes} overview={overview_state}"),
        );
        shot("after-dismissal").await;
        // A keysym reaches a client only when the session's layout has it:
        // `--xkb-layout no` gives the throwaway user a layout with æ.
        if let Some(layout) = &xkb_layout {
            let mut gsettings = tokio::process::Command::new("/usr/bin/gsettings");
            gsettings
                .args(["set", "org.gnome.desktop.input-sources", "sources"])
                .arg(format!("[('xkb', '{layout}')]"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            launcher
                .execution()
                .configure(&mut gsettings)
                .expect("user gsettings");
            let set = gsettings
                .status()
                .await
                .is_ok_and(|status| status.success());
            println!("HARNESS layout {layout} set={set}");
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        tokio::time::sleep(Duration::from_millis(settle_ms)).await;
        // The first key proves the injector, so it is sent only to a client
        // that holds keyboard focus. GNOME Shell may have closed its
        // startup overview on its own (escapes=0), leaving the keyboard on
        // the stage: the next key press then only moves focus to the window
        // (Mutter's focus-entering key) and the client sees its release
        // alone. A Shift press is that focus-entering key here.
        let focused = |log: &[String]| {
            log.iter()
                .rev()
                .find_map(|line| line.strip_prefix("focus in="))
                .is_some_and(|value| value == "True")
        };
        let focus_wait = std::time::Instant::now();
        let mut focus_how = "already focused";
        for shift in [false, true] {
            if shift {
                focus_how = "after a focus-entering Shift";
                input
                    .key_event(&key(QT_SHIFT, true, MOD_SHIFT))
                    .expect("shift");
                input.key_event(&key(QT_SHIFT, false, 0)).expect("shift");
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while !focused(&client_log) {
                match tokio::time::timeout_at(deadline, client_lines.next_line()).await {
                    Ok(Ok(Some(line))) => client_log.push(line),
                    _ => break,
                }
            }
            if focused(&client_log) {
                break;
            }
            focus_how = "never";
        }
        println!(
            "HARNESS focus before first key: {focus_how} ({} ms; overview escapes={escapes})",
            focus_wait.elapsed().as_millis()
        );
        tokio::time::sleep(step).await;
        if args.iter().any(|argument| argument == "--shift-warmup") {
            input
                .key_event(&key(QT_SHIFT, true, MOD_SHIFT))
                .expect("shift");
            input.key_event(&key(QT_SHIFT, false, 0)).expect("shift");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // The first key after the desktop became ready.
        for pressed in [true, false] {
            input.key_event(&key(0x41, pressed, 0)).expect("key a");
            tokio::time::sleep(step).await;
        }
        let at = |x: f64, y: f64| MouseMoveMsg {
            msg_type: "mouse_move".into(),
            x,
            y,
            ..MouseMoveMsg::default()
        };
        let button = |pressed: bool, x: f64, y: f64| MouseButtonMsg {
            msg_type: "mouse_button".into(),
            x,
            y,
            button: 1,
            pressed,
            ..MouseButtonMsg::default()
        };
        input.mouse_move(&at(0.5, 0.5)).expect("move");
        tokio::time::sleep(step).await;
        input.mouse_move(&at(0.52, 0.53)).expect("move");
        tokio::time::sleep(step).await;
        // Drag: press, move, release.
        input
            .mouse_button(&button(true, 0.52, 0.53))
            .expect("press");
        tokio::time::sleep(step).await;
        input.mouse_move(&at(0.6, 0.6)).expect("drag");
        tokio::time::sleep(step).await;
        input
            .mouse_button(&button(false, 0.6, 0.6))
            .expect("release");
        tokio::time::sleep(step).await;
        // Shift from the Deck's modifier mask: a capital B.
        input.key_event(&key(0x42, true, MOD_SHIFT)).expect("B");
        tokio::time::sleep(step).await;
        input.key_event(&key(0x42, false, 0)).expect("B");
        tokio::time::sleep(step).await;
        // Autorepeat: the Deck's repeated presses of a held key inject
        // nothing; the client repeats by itself while the key is held.
        input.key_event(&key(0x44, true, 0)).expect("d");
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            input.key_event(&key(0x44, true, 0)).expect("d repeat");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        input.key_event(&key(0x44, false, 0)).expect("d");
        tokio::time::sleep(step).await;
        // A Latin-1 key without an evdev code: its keysym.
        input.key_event(&key(0xC6, true, 0)).expect("ae");
        tokio::time::sleep(step).await;
        input.key_event(&key(0xC6, false, 0)).expect("ae");
        tokio::time::sleep(step).await;
        let scroll = |dx: f64, dy: f64, unit: ScrollUnitMsg| MouseScrollMsg {
            x: 0.6,
            y: 0.6,
            dx,
            dy,
            unit,
            ..MouseScrollMsg::default()
        };
        input
            .mouse_scroll(&scroll(0.0, 1.0, ScrollUnitMsg::Line))
            .expect("wheel up");
        tokio::time::sleep(step).await;
        input
            .mouse_scroll(&scroll(1.0, 0.0, ScrollUnitMsg::Line))
            .expect("wheel right");
        tokio::time::sleep(step).await;
        input
            .mouse_scroll(&scroll(0.0, 60.0, ScrollUnitMsg::Point))
            .expect("precise up");
        tokio::time::sleep(step).await;
        input
            .mouse_move_relative(&MouseMoveRelativeMsg {
                dx: 15,
                dy: -7,
                ..MouseMoveRelativeMsg::default()
            })
            .expect("relative");
        tokio::time::sleep(step).await;
        // Held key released when the input lane is lost: the lane worker's
        // `PeerLaneEvent::Lost` path calls `reset_held`.
        input.key_event(&key(0x45, true, 0)).expect("e");
        tokio::time::sleep(Duration::from_millis(300)).await;
        input.reset_held().expect("lane loss release");
        tokio::time::sleep(step).await;
        // Held key released when the attachment ends: the injector drops.
        input.key_event(&key(0x46, true, 0)).expect("f");
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(input);
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Supersession: a newer attachment's injector replaces an older one;
        // the session releases what the old one held and closes it.
        let (old, _) = WaylandInjector::connect(target.clone())
            .await
            .expect("old injector");
        let mut old = PierInput::Wayland(old);
        old.key_event(&key(0x47, true, 0)).expect("g");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (new, _) = WaylandInjector::connect(target.clone())
            .await
            .expect("new injector");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = old.key_event(&key(0x48, true, 0));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let old_lost = matches!(&old, PierInput::Wayland(injector) if injector.is_lost());
        let old_refused = old.key_event(&key(0x48, false, 0)).is_err();
        drop(old);
        drop(new);
        tokio::time::sleep(Duration::from_millis(300)).await;

        // A forced injector loss: the session agent stops answering (SIGSTOP)
        // for longer than a command may take. The injector reconnects with
        // the same target and proof once the agent answers again; input
        // refused meanwhile is counted, not delivered late.
        let agent = nix::unistd::Pid::from_raw(i32::try_from(launcher.agent_pid).unwrap_or(-1));
        let (lossy, lossy_stats) = WaylandInjector::connect(target.clone())
            .await
            .expect("injector");
        let mut lossy_status = lossy.status();
        let mut lossy = PierInput::Wayland(lossy);
        let _ = nix::sys::signal::kill(agent, nix::sys::signal::Signal::SIGSTOP);
        let stopped_at = std::time::Instant::now();
        let _ = lossy.key_event(&key(0x4A, true, 0));
        let _ = lossy.key_event(&key(0x4A, false, 0));
        let reconnecting = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(
                    *lossy_status.borrow_and_update(),
                    InjectorStatus::Reconnecting { .. }
                ) {
                    return true;
                }
                if lossy_status.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        let refused_while_down = matches!(
            lossy.key_event(&key(0x4C, true, 0)),
            Err(arcen_pier_linux::input::InputError::InjectorReconnecting)
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let _ = nix::sys::signal::kill(agent, nix::sys::signal::Signal::SIGCONT);
        let restored = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match &*lossy_status.borrow_and_update() {
                    InjectorStatus::Connected { connection } => return Some(*connection),
                    InjectorStatus::Lost(_) => return None,
                    InjectorStatus::Reconnecting { .. } => {}
                }
                if lossy_status.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await
        .ok()
        .flatten();
        let down_ms = stopped_at.elapsed().as_millis();
        if restored.is_some() {
            for pressed in [true, false] {
                let _ = lossy.key_event(&key(0x4B, pressed, 0));
                tokio::time::sleep(step).await;
            }
        }
        let unavailable_drops = lossy_stats.unavailable_drops();
        drop(lossy);
        tokio::time::sleep(Duration::from_millis(400)).await;

        // A lasting loss: the agent stays stopped beyond every reconnection
        // attempt, so the injector reports input lost for good (the Pier
        // then ends the attachment with that reason).
        let (gone, _) = WaylandInjector::connect(target.clone())
            .await
            .expect("injector");
        let mut gone_status = gone.status();
        let mut gone = PierInput::Wayland(gone);
        let _ = nix::sys::signal::kill(agent, nix::sys::signal::Signal::SIGSTOP);
        let _ = gone.key_event(&key(0x4D, true, 0));
        let lost_at = std::time::Instant::now();
        let lasting_loss = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                if let InjectorStatus::Lost(loss) = &*gone_status.borrow_and_update() {
                    return Some(loss.clone());
                }
                if gone_status.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await
        .ok()
        .flatten();
        let lost_ms = lost_at.elapsed().as_millis();
        let _ = nix::sys::signal::kill(agent, nix::sys::signal::Signal::SIGCONT);
        let lost_refused = matches!(
            gone.key_event(&key(0x4D, false, 0)),
            Err(arcen_pier_linux::input::InputError::InjectorLost)
        );
        drop(gone);
        tokio::time::sleep(Duration::from_millis(800)).await;

        // Refusals at the session boundary, on a raw connection.
        let socket = tokio::net::UnixStream::connect(&wayland.input_socket)
            .await
            .expect("input socket");
        let (reader, mut writer) = socket.into_split();
        let mut replies = BufReader::new(reader).lines();
        let mut raw = Vec::new();
        for command in [
            InputCommand::PointerAbsolute {
                output: "Meta-9".into(),
                generation: wayland.generation,
                x: 1.0,
                y: 1.0,
            },
            InputCommand::PointerAbsolute {
                output: stream.connector.clone(),
                generation: wayland.generation + 1,
                x: 1.0,
                y: 1.0,
            },
            InputCommand::PointerAbsolute {
                output: stream.connector.clone(),
                generation: wayland.generation,
                x: f64::from(stream.width) + 5.0,
                y: 1.0,
            },
            // Held across the raw disconnect: the session must release it.
            InputCommand::Key {
                keycode: 23,
                pressed: true,
            },
        ] {
            let mut line =
                serde_json::to_vec(&InputRequest::new(command.clone(), Duration::from_secs(5)))
                    .expect("encode");
            line.push(b'\n');
            writer.write_all(&line).await.expect("send input");
            let reply: InputReply = serde_json::from_str(
                &replies
                    .next_line()
                    .await
                    .expect("reply")
                    .expect("reply line"),
            )
            .expect("reply json");
            println!(
                "HARNESS raw {} -> {:?}",
                serde_json::to_string(&command).unwrap_or_default(),
                reply
            );
            raw.push(reply);
        }
        drop(writer);
        drop(replies);
        tokio::time::sleep(Duration::from_millis(800)).await;
        let refusals_ok = raw.len() == 4
            && raw[0].error.as_deref() == Some("unknown output")
            && raw[1].error.as_deref() == Some("stale output generation")
            && raw[2].error.as_deref() == Some("coordinate outside the output")
            && raw[3].ok;

        // A peer that is not the session's agent is refused by the Pier.
        let foreign =
            WaylandInputTarget::from_session(&wayland, uid, &session_id, 1).expect("target");
        let foreign_refused = matches!(
            WaylandInjector::connect(foreign).await,
            Err(WaylandInputError::ForeignPeer { .. })
        );
        tokio::time::sleep(Duration::from_millis(300)).await;

        let _ = gtk.start_kill();
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout(Duration::from_millis(500), client_lines.next_line()).await
        {
            client_log.push(line);
        }
        let _ = gtk.wait().await;
        for line in &client_log {
            println!("HARNESS client {line}");
        }
        let saw = |needle: &str| client_log.iter().any(|line| line.contains(needle));
        let count = |needle: &str| {
            client_log
                .iter()
                .filter(|line| line.contains(needle))
                .count()
        };
        let first_key = saw("key-press keyval=0x61") && saw("key-release keyval=0x61");
        all &= first_key;
        report(
            "first_key",
            first_key,
            &format!(
                "first key after the desktop became ready (dismiss={dismiss}): press={} release={}",
                saw("key-press keyval=0x61"),
                saw("key-release keyval=0x61")
            ),
        );
        let pointer =
            saw("motion") && saw("button-press button=1") && saw("button-release button=1");
        let drag = client_log
            .iter()
            .position(|line| line.contains("button-press button=1"))
            .zip(
                client_log
                    .iter()
                    .position(|line| line.contains("button-release button=1")),
            )
            .is_some_and(|(press, release)| {
                client_log[press..release]
                    .iter()
                    .any(|line| line.contains("motion"))
            });
        all &= pointer && drag;
        report(
            "pointer",
            pointer && drag,
            &format!(
                "absolute motion, button 1, drag with motion between press and release={drag}"
            ),
        );
        let modifiers = saw("key-press keyval=0x42");
        let repeats = count("key-press keyval=0x64");
        let autorepeat = repeats >= 2 && count("key-release keyval=0x64") >= 1;
        let keysym = saw("key-press keyval=0xe6") && saw("key-release keyval=0xe6");
        let keysym_ok = keysym || xkb_layout.is_none();
        all &= modifiers && autorepeat && keysym_ok;
        report(
            "keyboard",
            modifiers && autorepeat && keysym_ok,
            &format!("Shift+b gives B={modifiers}; held d pressed {repeats}x by client repeat (Deck repeats injected nothing); æ by keysym={keysym} (layout {xkb_layout:?}; a keysym the layout lacks is dropped by Mutter)"),
        );
        let wheel_up = client_log.iter().any(|line| {
            line.contains("scroll direction=up")
                || (line.contains("direction=smooth") && line.contains("dy=-"))
        });
        let wheel_right = client_log.iter().any(|line| {
            line.contains("scroll direction=right")
                || (line.contains("direction=smooth")
                    && line.contains("dx=")
                    && !line.contains("dx=0.000")
                    && !line.contains("dx=-"))
        });
        let scroll_ok = wheel_up && wheel_right;
        all &= scroll_ok;
        report(
            "scroll",
            scroll_ok,
            &format!("wheel up={wheel_up} right={wheel_right} (Deck +dy is up, +dx is right)"),
        );
        let lane_loss = saw("key-release keyval=0x65");
        let disconnect = saw("key-release keyval=0x66");
        let superseded = saw("key-release keyval=0x67") && old_lost && old_refused;
        let raw_disconnect = saw("key-release keyval=0x69");
        let release_ok = lane_loss && disconnect && superseded && raw_disconnect;
        all &= release_ok;
        report(
            "held_release",
            release_ok,
            &format!("lane loss={lane_loss} injector drop={disconnect} superseded={superseded} (old lost={old_lost} refused={old_refused}) raw disconnect={raw_disconnect}"),
        );
        // The key written just before the stall ('j') and the one typed
        // during it ('l') must never arrive: the session refuses a command
        // read after its deadline, and a waiting reconnection supersedes
        // the stalled connection before its leftover lines are read.
        let restored_ok = reconnecting
            && refused_while_down
            && restored.is_some()
            && saw("key-press keyval=0x6b")
            && saw("key-release keyval=0x6b")
            && !saw("key-press keyval=0x6a")
            && !saw("key-press keyval=0x6c");
        all &= restored_ok;
        report(
            "injector_loss",
            restored_ok,
            &format!(
                "agent stopped {down_ms} ms: reconnecting={reconnecting}, input refused while down={refused_while_down} (counted {unavailable_drops}), restored on connection {restored:?}, 'k' after restore delivered={}, pre-stall 'j' never delivered={}, 'l' typed while down never delivered={}",
                saw("key-press keyval=0x6b"),
                !saw("key-press keyval=0x6a"),
                !saw("key-press keyval=0x6c")
            ),
        );
        let stale_m = saw("key-press keyval=0x6d");
        let lasting_ok =
            matches!(lasting_loss, Some(InjectorLoss::Unreachable(_))) && lost_refused && !stale_m;
        all &= lasting_ok;
        report(
            "injector_lost",
            lasting_ok,
            &format!("agent stopped beyond every attempt: {lasting_loss:?} after {lost_ms} ms; later input refused as lost={lost_refused}; 'm' written before the stall never delivered={}", !stale_m),
        );
        let refusal_ok = refusals_ok && foreign_refused;
        all &= refusal_ok;
        report(
            "refusals",
            refusal_ok,
            &format!("foreign output, stale generation, out of bounds refused={refusals_ok}; foreign peer pid refused by the Pier={foreign_refused}"),
        );

        // On the desktop, after the input stages: the Activities overview keeps
        // the pointer for GNOME Shell.
        if x11_pointer {
            let proof = match (&x11_ready, wayland.streams.first()) {
                (Some(ready_x11), Some(stream)) => {
                    let target = arcen_pier_linux::session::wayland::xwayland::PointerTarget {
                        input_socket: wayland.input_socket.clone(),
                        connector: stream.connector.clone(),
                        generation: wayland.generation,
                        rect: (stream.x, stream.y, stream.width, stream.height),
                    };
                    let runtime_dir = wayland.runtime_dir.clone();
                    let displays = ready_x11.displays;
                    let auth = ready_x11.xauthority.clone();
                    #[allow(clippy::disallowed_methods)]
                    let task = tokio::task::spawn_blocking(move || {
                        arcen_pier_linux::session::wayland::xwayland::prove_pointer(
                            &arcen_pier_linux::session::wayland::xwayland::X11Dirs::production(),
                            uid,
                            &runtime_dir,
                            displays.map_or(0, |displays| displays.public),
                            &auth.unwrap_or_default(),
                            &target,
                            Duration::from_secs(2),
                        )
                    });
                    task.await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result)
                }
                _ => Err("no X11 session".to_owned()),
            };
            let followed = proof.as_ref().is_ok_and(|proof| proof.followed());
            all &= followed;
            report("x11_pointer", followed, &format!("{proof:?}"));
        }
        drop(consumer);
        let scope = format!(
            "/sys/fs/cgroup/user.slice/user-{}.slice/session-{session_id}.scope",
            launcher.identity.uid
        );
        if kill_compositor {
            let procs =
                std::fs::read_to_string(format!("{scope}/cgroup.procs")).unwrap_or_default();
            let shell = procs.lines().find(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|comm| comm.trim() == "gnome-shell" || comm.trim() == "mutter")
            });
            if let Some(pid) = shell {
                let _ = run_text("/usr/bin/kill", &["-KILL", pid]).await;
                // The Pier watches the agent; the launcher ends the logind
                // session itself once the agent has torn the desktop down.
                let killed_at = std::time::Instant::now();
                let mut ended = false;
                for _ in 0..120 {
                    let listed = run_text("/usr/bin/loginctl", &["list-sessions", "--no-legend"])
                        .await
                        .lines()
                        .any(|line| line.split_whitespace().next() == Some(session_id.as_str()));
                    if !launcher.agent_is_running() && !listed {
                        ended = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                all &= ended;
                report(
                    "compositor_death",
                    ended,
                    &format!(
                        "killed pid={pid}; agent gone and logind session {session_id} ended={ended} after_ms={} runtime_dir_gone={}",
                        killed_at.elapsed().as_millis(),
                        !wayland.runtime_dir.exists()
                    ),
                );
            } else {
                all = false;
                report("compositor_death", false, "no compositor process found");
            }
        }

        if terminate_session {
            // What an administrator's `loginctl terminate-session` or a
            // user logout does to the session.
            let _ = run_text("/usr/bin/loginctl", &["terminate-session", &session_id]).await;
            let started = std::time::Instant::now();
            let mut ended = false;
            for _ in 0..120 {
                let listed = run_text("/usr/bin/loginctl", &["list-sessions", "--no-legend"])
                    .await
                    .lines()
                    .any(|line| line.split_whitespace().next() == Some(session_id.as_str()));
                if !launcher.agent_is_running() && !listed {
                    ended = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            all &= ended;
            let leftover = std::fs::read_dir(&wayland.runtime_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                })
                .ok();
            report(
                "logout",
                ended,
                &format!(
                    "terminate-session {session_id}: agent gone and session ended={ended} after_ms={} runtime_dir_left={leftover:?}",
                    started.elapsed().as_millis()
                ),
            );
        }

        let agent_exited = launcher.shutdown().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(displays) = x11_ready.as_ref().and_then(|x11| x11.displays) {
            let left = arcen_pier_linux::session::wayland::xwayland::session_artifacts_present(
                &arcen_pier_linux::session::wayland::xwayland::X11Dirs::production(),
                uid,
                &displays,
            );
            let none_left = left.is_empty();
            all &= none_left;
            report(
                "x11_teardown",
                none_left,
                &format!(
                    "displays {:?} (compositor pid {}): sockets and lock files left {left:?}",
                    displays.numbers(),
                    displays.lock_pid
                ),
            );
        }
        let session_gone = !run_text("/usr/bin/loginctl", &["list-sessions", "--no-legend"])
            .await
            .lines()
            .any(|line| line.split_whitespace().next() == Some(session_id.as_str()));
        let scope_gone = !std::path::Path::new(&scope).exists();
        let runtime_gone = !wayland.runtime_dir.exists();
        let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        let no_mounts = !mounts.contains(&wayland.runtime_dir.to_string_lossy().into_owned());
        let user_processes = run_text("/usr/bin/pgrep", &["-u", &user, "-a"]).await;
        let session_processes = user_processes
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        let clean = agent_exited && session_gone && scope_gone && runtime_gone && no_mounts;
        all &= clean;
        // logind stops the user manager (and the user's PulseAudio with it)
        // once the user's last session has gone, after UserStopDelaySec.
        let mut manager_stopped = false;
        for _ in 0..60 {
            if run_text("/usr/bin/pgrep", &["-u", &user])
                .await
                .trim()
                .is_empty()
            {
                manager_stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        all &= manager_stopped;
        report(
            "user_manager",
            manager_stopped,
            &format!(
                "no process of uid {uid} remains after the user manager stopped={manager_stopped} runtime_dir_present={}",
                std::path::Path::new(&format!("/run/user/{uid}")).exists()
            ),
        );
        report(
            "teardown",
            clean,
            &format!(
                "agent_exited={agent_exited} logind_session_gone={session_gone} scope_gone={scope_gone} runtime_dir_gone={runtime_gone} no_mounts={no_mounts} remaining_user_processes={session_processes:?}"
            ),
        );
        all
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    if runtime.block_on(harness::main()) {
        println!("HARNESS RESULT: PASS");
        std::process::ExitCode::SUCCESS
    } else {
        println!("HARNESS RESULT: FAIL");
        std::process::ExitCode::FAILURE
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the headless Wayland session harness runs on Linux only");
}
