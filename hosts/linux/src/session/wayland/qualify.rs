//! `arcen-pier wayland qualify`: proves this host's headless Wayland facts
//! with a real session and records them.
//!
//! A Pier selects its desktop provider at start, before any session exists,
//! so the facts only a session can prove — that a capture route delivered
//! an encoded frame, that session-bound input injection works, and how fast
//! the compositor delivers frames for Speed — come from this command. It
//! runs the product machinery end to end (PAM and the privileged launcher,
//! the session agent's headless compositor, capenc's Wayland Auto and Speed
//! compositions through the Pier's own capenc supervision, the Pier's
//! session-bound injector), measures, tears everything down, and with
//! `--write` records the result at [`super::qualification::RECORD_PATH`].
//!
//! **Outputs.** Each `--output WxH@R` names a Deck display. The session is
//! created as the Pier would create it for a Speed request on that display:
//! at the shared paced refresh (`paced_output_refresh_hz`, 120 Hz for a
//! 60 Hz display), and the Speed rate is recorded for that output refresh.
//!
//! **GPU import.** Before the Auto run, capenc's capture harness verifies the
//! `gpu-convert` DMA-BUF route on the session's still desktop
//! (`wayland-capture-probe verify-gpu-import`: a GPU-imported frame read back
//! and compared with the host-copy frame of the same desktop). Only a
//! verified route is offered to the Auto and Speed runs and recorded as GPU
//! import; otherwise they run, and are recorded, on the host copy.
//!
//! **Who the session runs as.** By default a temporary local account,
//! `arcen-wlq`, created for the run and removed afterwards: a regular uid
//! (logind gives it a `user` session with a user manager and `PulseAudio`),
//! no login shell, a random password that exists only in this process and
//! the shadow file for the length of the run, and a fresh home that is
//! deleted with it. No directory or network account is involved and no
//! real user's desktop, settings or files are touched. `--user NAME
//! --password-file FILE` uses an operator-named local test account instead;
//! it must be in `/etc/passwd` (never a directory account) and have no
//! logged-in session.
//!
//! Root only. Refuses to run while `arcen-pier.service` is active: a
//! qualification session would compete with a served one for the encoder.
//!
//! **Which record.** `--write` writes the configured record
//! (`capture.wayland_qualification_record`, default the production
//! record). A development Pier names its own (`--record PATH`, or its
//! configuration); such a run uses its own temporary account
//! (`arcen-wlq-<digest of the path>`, or `--temporary-user arcen…`) and
//! transient scope, never creates or removes `arcen-wlq`, and may run beside
//! the active production service only with `--allow-running-service`
//! (refused for the production record). See
//! `arcen_session::wayland_qualification::QualificationTarget`.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arcen_media::video::{
    paced_output_refresh_hz, pipeline_contract, CaptureMemory, CopyPath, MotionPriority,
    PipelineId, SustainedRateMeter,
};
use arcen_protocol::messages::{ClientMonitor, CursorMode, VideoSelectionIntent};
use arcen_session::desktop_provider::{
    select_desktop_provider, CapabilityFact, DesktopProvider, HostDesktopFacts, ProviderPreference,
};
use arcen_session::wayland_qualification::{
    qualification_outputs, speed_minimum_fps, CaptureRoute, CaptureRouteProof, EncoderProof,
    HostIdentity, InputProof, QualificationTarget, QualificationTargetError, QualifiedFacts,
    QualifyOutput, RateEvidence, SourceRateMeasurement, SpeedRequest, WaylandQualificationRecord,
    MAX_QUALIFY_OUTPUTS, QUALIFY_CODECS, SCHEMA_VERSION, SPEED_QUALIFICATION_LADDER,
};
use arcen_session::wayland_session::{
    FrameEvidence, FrameImport, FrameMemory, HeadlessCompositor, RemoteDesktopEvidence,
    ScreenCastStream, VerifiedOutput, WaylandSessionReadiness,
};
use arcen_session::wayland_x11::{GlamorRoute, XwaylandProof, XWAYLAND_BINARY};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use zeroize::Zeroizing;

use super::qualification;

mod multi_output;
use super::{InputCommand, InputReply, InputRequest, WaylandAgentReady};

mod fidelity;
use crate::input::wayland::{WaylandInjector, WaylandInputTarget};
use crate::media::capenc::{self, CapencConfig, EncoderSelection};
use crate::session::launcher::{AuthenticatedLauncher, LauncherConfig, SessionLauncher};

const DEFAULT_CONFIG: &str = "/etc/arcen/pier.json";
const PIER_SERVICE: &str = "arcen-pier.service";
const USERADD: &str = "/usr/sbin/useradd";
const USERDEL: &str = "/usr/sbin/userdel";
const CHPASSWD: &str = "/usr/sbin/chpasswd";
const LOGINCTL: &str = "/usr/bin/loginctl";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const PACTL: &str = "/usr/bin/pactl";
const PASSWD: &str = "/etc/passwd";
const LOGIN_DEFS: &str = "/etc/login.defs";
const DEFAULT_MEASURE_SECONDS: u32 = 8;
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
/// Marks the re-executed command inside its transient scope.
const IN_SERVICE_FLAG: &str = "--in-service";
const AUTO_RUN: Duration = Duration::from_secs(3);
/// Pointer motion rate: content that changes at Speed's own rate, as a
/// 60 fps video or game does. Driving it faster measures the compositor's
/// pacing under a load Speed never serves (on GNOME 49 a 120 Hz output
/// recorded 44-51 fps of 120 Hz cursor motion but every frame of 60 Hz
/// content).
fn motion_hz() -> u32 {
    pipeline_contract(PipelineId::Speed).max_fps
}
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const TEARDOWN_WAIT: Duration = Duration::from_secs(20);
/// Attempts at the GPU import check: a desktop still settling after login
/// (the overview animation) differs between the two captures.
const GPU_IMPORT_ATTEMPTS: u32 = 3;
const GPU_IMPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// The output refresh a session for Speed on a `display_refresh_hz` display
/// is created at (the Pier's rule, `Config::wayland_session_refresh_hz`).
fn speed_output_refresh_hz(display_refresh_hz: u32) -> u32 {
    paced_output_refresh_hz(
        display_refresh_hz,
        pipeline_contract(PipelineId::Speed).max_fps,
    )
    .min(arcen_session::wayland_session::MAX_VIRTUAL_OUTPUT_REFRESH_HZ)
}

/// What capenc's `wayland-capture-probe verify-gpu-import` reported.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GpuImportVerification {
    /// `ok`, `MISMATCH`, `UNDISCRIMINATING` or `UNVERIFIED`.
    result: String,
    /// GPU-imported frames before the comparison.
    gpu_frames: u64,
    /// Pixels compared and those that differed.
    pixels: u64,
    mismatched: u64,
    /// `explicit` or `implicit`.
    sync: String,
}

impl GpuImportVerification {
    fn verified(&self) -> bool {
        self.result == "ok" && self.gpu_frames > 0 && self.pixels > 0
    }
}

/// Parses the harness's `wayland-probe gpu-import-verify …` line.
fn parse_gpu_import_verify(line: &str) -> Option<GpuImportVerification> {
    let rest = line.split_once("wayland-probe gpu-import-verify ")?.1;
    let field = |key: &str| {
        rest.split_whitespace()
            .find_map(|token| token.strip_prefix(key)?.strip_prefix('='))
    };
    let number = |key: &str| field(key).and_then(|value| value.parse().ok());
    let result = field("result")?.to_owned();
    if result == "UNVERIFIED" {
        return Some(GpuImportVerification {
            result,
            gpu_frames: 0,
            pixels: 0,
            mismatched: 0,
            sync: "none".to_owned(),
        });
    }
    Some(GpuImportVerification {
        result,
        gpu_frames: number("gpu_frames")?,
        pixels: number("pixels")?,
        mismatched: number("mismatched")?,
        sync: field("sync")?.to_owned(),
    })
}

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifyOptions {
    /// Write the record.
    pub write: bool,
    /// An operator-named local account and its password file, instead of
    /// the temporary one.
    pub user: Option<(String, PathBuf)>,
    /// Deck displays the operator named (`--output`); measured first, with
    /// the previous record's, those Decks requested, and the default
    /// ladder ([`qualification_outputs`]).
    pub outputs: Vec<QualifyOutput>,
    /// `--replace-outputs`: forget the previous record's and the requested
    /// displays; measure only the operator's (or the ladder).
    pub replace_outputs: bool,
    /// Codecs Speed is measured with (`--codec`, default [`QUALIFY_CODECS`]).
    pub codecs: Vec<String>,
    /// Moving phase of each Speed measurement.
    pub seconds: u32,
    /// Pier configuration (PAM service, desktop session, encoder settings).
    pub config: Option<PathBuf>,
    /// Log the Pier's own diagnostics to stderr.
    pub verbose: bool,
    /// Already running outside any login session (internal).
    pub in_service: bool,
    /// `--record PATH`: the record to write instead of the configured one
    /// (`capture.wayland_qualification_record`, else production).
    pub record: Option<String>,
    /// `--temporary-user NAME`: the temporary account for a non-production
    /// record (default: derived from the record path).
    pub temporary_user: Option<String>,
    /// `--allow-running-service`: run beside an active Pier service; only
    /// for a non-production record.
    pub allow_running_service: bool,
    /// `--multi-output N`: outputs captured concurrently in one session to
    /// prove multi-monitor (2..=4; 0 skips; default 4, then 2 if 4 fails).
    pub multi_outputs: usize,
}

/// Usage text.
pub const USAGE: &str = "usage: arcen-pier wayland qualify [--write] \
[--user NAME --password-file FILE] [--output WIDTHxHEIGHT@HZ]... [--replace-outputs] \
[--codec h265|h264|av1]... [--seconds N] \
[--config PATH | --no-config] [--record PATH [--temporary-user NAME] \
[--allow-running-service]] [--multi-output N] [--verbose]
       arcen-pier wayland eis-probe --socket PATH";

impl QualifyOptions {
    /// Parses the arguments after `wayland qualify`.
    ///
    /// # Errors
    ///
    /// The first unknown, repeated or malformed argument.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            write: false,
            user: None,
            outputs: Vec::new(),
            replace_outputs: false,
            codecs: Vec::new(),
            seconds: DEFAULT_MEASURE_SECONDS,
            config: Path::new(DEFAULT_CONFIG)
                .is_file()
                .then(|| PathBuf::from(DEFAULT_CONFIG)),
            verbose: false,
            in_service: false,
            record: None,
            temporary_user: None,
            allow_running_service: false,
            multi_outputs: multi_output::DEFAULT_MULTI_OUTPUTS,
        };
        let mut user = None;
        let mut password_file = None;
        let mut config_seen = false;
        let mut arguments = args.iter();
        while let Some(argument) = arguments.next() {
            let mut value = |name: &str| {
                arguments
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match argument.as_str() {
                "--write" => options.write = true,
                "--verbose" | "-v" => options.verbose = true,
                IN_SERVICE_FLAG => options.in_service = true,
                "--user" if user.is_none() => user = Some(value("--user")?),
                "--password-file" if password_file.is_none() => {
                    password_file = Some(PathBuf::from(value("--password-file")?));
                }
                "--output" => {
                    let output = value("--output")?;
                    let parsed = QualifyOutput::parse(&output)
                        .ok_or_else(|| format!("invalid --output {output:?}"))?;
                    if !options.outputs.contains(&parsed) {
                        options.outputs.push(parsed);
                    }
                }
                "--replace-outputs" => options.replace_outputs = true,
                "--codec" => {
                    let codec = value("--codec")?;
                    if !matches!(codec.as_str(), "h264" | "h265" | "av1") {
                        return Err(format!("--codec must be h264, h265 or av1, not {codec:?}"));
                    }
                    if !options.codecs.contains(&codec) {
                        options.codecs.push(codec);
                    }
                }
                "--seconds" => {
                    let seconds = value("--seconds")?;
                    options.seconds = seconds
                        .parse()
                        .ok()
                        .filter(|seconds| (4..=60).contains(seconds))
                        .ok_or_else(|| format!("--seconds must be 4..60, not {seconds:?}"))?;
                }
                "--config" if !config_seen => {
                    config_seen = true;
                    options.config = Some(PathBuf::from(value("--config")?));
                }
                "--no-config" if !config_seen => {
                    config_seen = true;
                    options.config = None;
                }
                "--record" if options.record.is_none() => {
                    options.record = Some(value("--record")?);
                }
                "--temporary-user" if options.temporary_user.is_none() => {
                    options.temporary_user = Some(value("--temporary-user")?);
                }
                "--allow-running-service" => options.allow_running_service = true,
                "--multi-output" => {
                    let count = value("--multi-output")?;
                    options.multi_outputs = count
                        .parse::<usize>()
                        .ok()
                        .filter(|count| *count == 0 || (2..=4).contains(count))
                        .ok_or_else(|| {
                            format!("--multi-output must be 0 or 2..4, not {count:?}")
                        })?;
                }
                other => return Err(format!("unexpected argument {other:?}\n{USAGE}")),
            }
        }
        options.user = match (user, password_file) {
            (None, None) => None,
            (Some(user), Some(file)) => Some((user, file)),
            _ => return Err("--user and --password-file go together".to_owned()),
        };
        if options.temporary_user.is_some() && options.user.is_some() {
            return Err("--temporary-user and --user exclude each other".to_owned());
        }
        if options.codecs.is_empty() {
            options.codecs = QUALIFY_CODECS
                .iter()
                .map(|&codec| codec.to_owned())
                .collect();
        }
        Ok(options)
    }

    /// The record this run writes and the names it uses: `--record` (with
    /// `--temporary-user`), else the configuration's
    /// `capture.wayland_qualification_record`, else production. A
    /// `--temporary-user` needs a `--record`.
    ///
    /// # Errors
    ///
    /// An invalid record path or account name.
    pub fn target(
        &self,
        configured: &QualificationTarget,
    ) -> Result<QualificationTarget, QualificationTargetError> {
        match (&self.record, &self.temporary_user) {
            (Some(record), user) => QualificationTarget::for_record(record, user.as_deref()),
            (None, Some(user)) => {
                QualificationTarget::for_record(configured.record_path(), Some(user.as_str()))
            }
            (None, None) => Ok(configured.clone()),
        }
    }
}

/// The run's target from the options and the configuration.
fn resolve_target(options: &QualifyOptions) -> Result<QualificationTarget, String> {
    let configured = load_config(options)?.wayland_qualification_target;
    options
        .target(&configured)
        .map_err(|error| error.to_string())
}

/// `arcen-pier wayland …`.
#[must_use]
pub fn main(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("qualify") => {}
        Some("eis-probe") => return eis_probe_main(&args[1..]),
        Some("--help" | "-h") | None => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("unknown wayland command {other:?}\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    let options = match QualifyOptions::parse(&args[1..]) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let target = match resolve_target(&options) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("qualify: {error}");
            return ExitCode::from(2);
        }
    };
    if !options.in_service && in_login_session() {
        return run_outside_login_session(&args[1..], &target);
    }
    let filter = if options.verbose { "info" } else { "warn" };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .try_init();
    crate::net::tls::init_crypto();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("qualify: tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(options, target)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("qualify: {error}");
            ExitCode::from(2)
        }
    }
}

/// `arcen-pier wayland eis-probe --socket PATH`: asks the EIS server at
/// `PATH` (a libei socket, such as libei's own `eis-demo-server`) whether its
/// seat offers a stylus, and prints the session facts it would yield as
/// JSON. Injects nothing.
fn eis_probe_main(args: &[String]) -> ExitCode {
    let socket = match args {
        [flag, path] if flag == "--socket" => std::path::PathBuf::from(path),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("eis-probe: tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(super::eis::probe_path(&socket)) {
        Ok(evidence) => {
            let report = serde_json::json!({
                "seat": evidence.seat_name,
                "negotiated": evidence.negotiated,
                "stylus": evidence.stylus_facts(),
            });
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("eis-probe: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Whether this process belongs to a logind session (an operator's shell).
/// `pam_systemd` opens no session for a caller that already has one, so the
/// qualification session must start where the Pier's do: outside any login.
fn in_login_session() -> bool {
    std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|cgroup| {
        cgroup.lines().any(|line| {
            line.rsplit('/')
                .next()
                .is_some_and(|leaf| leaf.starts_with("session-") && leaf.ends_with(".scope"))
        })
    })
}

/// Re-runs this command in a transient scope outside the operator's login
/// session, on this terminal. A scope keeps the operator's own security
/// context, which `pam_selinux` may enter the session user's context from;
/// a transient *service* would run as a service domain, which SELinux lets
/// transition into a user's context only with Arcen's policy module
/// (`docs/security/linux-selinux.md`).
fn run_outside_login_session(args: &[String], target: &QualificationTarget) -> ExitCode {
    let Some(executable) = crate::current_pier_exe() else {
        eprintln!("qualify: the Pier executable cannot be resolved");
        return ExitCode::FAILURE;
    };
    let status = std::process::Command::new(SYSTEMD_RUN)
        .args([
            "--scope",
            "--unit",
            target.scope_unit(),
            "--collect",
            "--quiet",
            "--",
        ])
        .arg(executable)
        .args(["wayland", "qualify"])
        .args(args)
        .arg(IN_SERVICE_FLAG)
        .status();
    match status {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => {
            eprintln!("qualify: {SYSTEMD_RUN}: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Where an automatic run (the Pier service re-qualifying itself) also
/// sends its step lines: the managed log, and the failing ones for its
/// attempt marker.
static AUTOMATIC_FAILURES: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

fn step(name: &str, ok: bool, detail: &str) {
    let line = format!("{} {name}: {detail}", if ok { "ok  " } else { "FAIL" });
    println!("{line}");
    let mut automatic = AUTOMATIC_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(failures) = automatic.as_mut() {
        if ok {
            tracing::info!(target: crate::logging::target::SESSION, "automatic Wayland re-qualification: {line}");
        } else {
            tracing::warn!(target: crate::logging::target::SESSION, "automatic Wayland re-qualification: {line}");
            failures.push(line);
        }
    }
}

/// How a qualification was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `arcen-pier wayland qualify` by an operator or the installer.
    Command,
    /// The Pier service, at start, for a record an update made stale:
    /// bounded, and never beside a running service (it is the service).
    Automatic,
}

/// What an automatic re-qualification found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomaticOutcome {
    /// Whether it wrote a record that serves Wayland.
    pub passed: bool,
    /// The failing steps.
    pub failures: Vec<String>,
    /// A stop was requested before it had an outcome: its desktop and
    /// temporary account were removed and nothing was recorded.
    pub interrupted: bool,
}

/// Whether an automatic run has used its time
/// (`AUTO_REQUALIFY_DEADLINE_S`): what it has not started then — later
/// displays, the multi-output runs — is left for the next qualification. A
/// command run is unbounded.
fn past_automatic_deadline(mode: Mode, elapsed: Duration) -> bool {
    mode == Mode::Automatic
        && elapsed
            >= Duration::from_secs(arcen_session::wayland_qualification::AUTO_REQUALIFY_DEADLINE_S)
}

/// The Pier service re-qualifies its own record at start (see
/// `arcen_session::wayland_qualification::auto_requalify_decision`): the
/// same session, measurements and record as the command, bounded to
/// `AUTO_REQUALIFY_MAX_OUTPUTS` displays and `AUTO_REQUALIFY_DEADLINE_S`;
/// displays it leaves out stay in the requested-displays list.
pub async fn run_automatic(
    config: crate::cli::Config,
    target: QualificationTarget,
    stop: tokio::sync::watch::Receiver<crate::service_signals::StopRequest>,
) -> AutomaticOutcome {
    *AUTOMATIC_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
    let options = QualifyOptions {
        write: true,
        user: None,
        outputs: Vec::new(),
        replace_outputs: false,
        codecs: QUALIFY_CODECS
            .iter()
            .map(|&codec| codec.to_owned())
            .collect(),
        seconds: arcen_session::wayland_qualification::AUTO_REQUALIFY_MEASURE_SECONDS,
        config: None,
        verbose: false,
        in_service: true,
        record: None,
        temporary_user: None,
        allow_running_service: false,
        // The multi-monitor offer rests on it.
        multi_outputs: multi_output::DEFAULT_MULTI_OUTPUTS,
    };
    let result = run_with(options, target, config, Mode::Automatic, Some(stop.clone())).await;
    let mut failures = AUTOMATIC_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_default();
    let interrupted = result.is_err() && stop.borrow().is_some();
    let passed = match result {
        Ok(passed) => passed,
        Err(error) => {
            failures.push(format!("qualify: {error}"));
            false
        }
    };
    AutomaticOutcome {
        passed,
        failures,
        interrupted,
    }
}

async fn command_output(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| format!("{program} timed out"))?
    .map_err(|error| format!("{program}: {error}"))
}

async fn run_checked(program: &str, args: &[&str]) -> Result<(), String> {
    let output = command_output(program, args).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The local `/etc/passwd` entry of `name`: `(uid, gecos)`.
fn local_account(name: &str) -> Option<(u32, String)> {
    let passwd = std::fs::read_to_string(PASSWD).ok()?;
    passwd.lines().find_map(|line| {
        let fields = line.split(':').collect::<Vec<_>>();
        (fields.len() >= 7 && fields[0] == name)
            .then(|| Some((fields[2].parse().ok()?, fields[4].to_owned())))
            .flatten()
    })
}

async fn has_sessions(name: &str) -> bool {
    command_output(LOGINCTL, &["show-user", name, "-p", "Sessions", "--value"])
        .await
        .is_ok_and(|output| {
            output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        })
}

async fn user_manager_gone(name: &str) -> bool {
    let deadline = Instant::now() + TEARDOWN_WAIT;
    loop {
        let known = command_output(LOGINCTL, &["show-user", name, "-p", "State", "--value"])
            .await
            .is_ok_and(|output| output.status.success());
        if !known {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The account the qualification session runs as.
struct QualificationUser {
    name: String,
    password: Zeroizing<String>,
    ephemeral: bool,
    target: QualificationTarget,
}

fn random_password() -> Result<Zeroizing<String>, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut bytes = Zeroizing::new([0_u8; 32]);
    getrandom::getrandom(bytes.as_mut()).map_err(|error| format!("random password: {error}"))?;
    Ok(Zeroizing::new(
        bytes
            .iter()
            .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
            .collect(),
    ))
}

/// Removes this target's temporary account, and only an account carrying
/// this target's marker.
async fn remove_ephemeral_user(target: &QualificationTarget) -> Result<(), String> {
    let name = target.temporary_user();
    match local_account(name) {
        None => return Ok(()),
        Some((_, gecos)) if gecos != target.temporary_user_gecos() => {
            return Err(format!(
                "{name} was not created by a qualification for {}; left in place",
                target.record_path()
            ));
        }
        Some(_) => {}
    }
    let _ = command_output(LOGINCTL, &["terminate-user", name]).await;
    // logind can keep a killed run's user `closing` with no process left
    // until it restarts. The account goes anyway (`userdel` refuses one that
    // still runs a process), and the next account takes another UID
    // (`temporary_uid`), so that stale user never blocks a qualification.
    let manager_gone = user_manager_gone(name).await;
    let mut last = String::new();
    for _ in 0..10 {
        match run_checked(USERDEL, &["--remove", name]).await {
            Ok(()) => break,
            Err(error) => last = error,
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if local_account(name).is_some() {
        return Err(if manager_gone {
            format!("could not remove {name}: {last}")
        } else {
            format!(
                "{name} still has a logind user after {}s and could not be removed: {last}",
                TEARDOWN_WAIT.as_secs()
            )
        });
    }
    if !manager_gone {
        step(
            "user",
            true,
            &format!(
                "{name} removed; logind still lists its user (closing, no process); the next \
                 temporary account takes another UID"
            ),
        );
    }
    Ok(())
}

/// Whether logind or the host still holds `uid`: a user logind has not
/// collected, a runtime directory left behind, or an account from a
/// directory service.
fn uid_still_held(uid: u32) -> bool {
    Path::new(&format!("/run/systemd/users/{uid}")).exists()
        || Path::new(&format!("/run/user/{uid}")).exists()
        || nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .ok()
            .flatten()
            .is_some()
}

/// The UID of a new temporary account
/// (`arcen_session::wayland_qualification::temporary_account_uid`).
fn temporary_uid() -> Result<u32, String> {
    use arcen_session::wayland_qualification::{
        login_uid_range, temporary_account_uid, DEFAULT_LOGIN_UID_RANGE,
    };
    let passwd = std::fs::read_to_string(PASSWD).map_err(|error| format!("{PASSWD}: {error}"))?;
    let range = std::fs::read_to_string(LOGIN_DEFS)
        .map_or(DEFAULT_LOGIN_UID_RANGE, |defs| login_uid_range(&defs));
    let uid = temporary_account_uid(&passwd, &range, uid_still_held).ok_or_else(|| {
        format!(
            "no UID left in {}..={} for the temporary account",
            range.start(),
            range.end()
        )
    })?;
    if let Some(first) = temporary_account_uid(&passwd, &range, |_| false) {
        if first != uid {
            let held = if uid - first == 1 {
                format!("UID {first} is")
            } else {
                format!("UIDs {first}-{} are", uid - 1)
            };
            step(
                "user",
                true,
                &format!(
                    "{held} still held (a logind user left closing by an interrupted run); the \
                     temporary account takes {uid}"
                ),
            );
        }
    }
    Ok(uid)
}

impl QualificationUser {
    async fn prepare(
        options: &QualifyOptions,
        target: &QualificationTarget,
    ) -> Result<Self, String> {
        if let Some((name, password_file)) = &options.user {
            let Some((uid, _)) = local_account(name) else {
                return Err(format!(
                    "{name} is not a local account in {PASSWD}; qualification never runs as a \
                     directory (AD/LDAP) user"
                ));
            };
            if uid < 1000 || uid == 65_534 {
                return Err(format!("{name} is a system account (uid {uid})"));
            }
            if has_sessions(name).await {
                return Err(format!(
                    "{name} is logged in; qualification never touches a user's running desktop"
                ));
            }
            let password = Zeroizing::new(
                std::fs::read_to_string(password_file)
                    .map_err(|error| format!("{}: {error}", password_file.display()))?
                    .trim_end_matches(['\r', '\n'])
                    .to_owned(),
            );
            return Ok(Self {
                name: name.clone(),
                password,
                ephemeral: false,
                target: target.clone(),
            });
        }
        let ephemeral_user = target.temporary_user();
        if let Some((_, gecos)) = local_account(ephemeral_user) {
            if gecos != target.temporary_user_gecos() {
                return Err(format!(
                    "a local account {ephemeral_user} exists that this command did not create; \
                     remove it or use --user"
                ));
            }
            step(
                "user",
                true,
                &format!("removing {ephemeral_user} left by an interrupted qualification"),
            );
            remove_ephemeral_user(target).await?;
        }
        let uid = temporary_uid()?.to_string();
        run_checked(
            USERADD,
            &[
                "--create-home",
                "--uid",
                &uid,
                "--comment",
                target.temporary_user_gecos(),
                "--shell",
                "/sbin/nologin",
                ephemeral_user,
            ],
        )
        .await?;
        let password = random_password()?;
        let set = async {
            let mut child = Command::new(CHPASSWD)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| format!("{CHPASSWD}: {error}"))?;
            let mut line = Zeroizing::new(format!("{ephemeral_user}:{}\n", password.as_str()));
            let mut stdin = child.stdin.take().ok_or("chpasswd stdin")?;
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|error| format!("{CHPASSWD}: {error}"))?;
            line.clear();
            drop(stdin);
            let output = child
                .wait_with_output()
                .await
                .map_err(|error| format!("{CHPASSWD}: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "{CHPASSWD} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        };
        if let Err(error) = set.await {
            let _ = remove_ephemeral_user(target).await;
            return Err(error);
        }
        Ok(Self {
            name: ephemeral_user.to_owned(),
            password,
            ephemeral: true,
            target: target.clone(),
        })
    }

    async fn release(&self) -> Result<(), String> {
        if self.ephemeral {
            remove_ephemeral_user(&self.target).await
        } else {
            Ok(())
        }
    }
}

/// What a native encoder probe's `UNAVAILABLE` says about the host. Only
/// an answer that no native encoder exists here is recorded as missing: no
/// encoder hardware or none built in, or no NVIDIA runtime on a host with
/// no NVIDIA driver loaded at all (Intel or AMD graphics). An incomplete or
/// mismatched NVIDIA stack (`runtime_missing` beside a loaded driver), a
/// session limit, or a display/configuration refusal is unknown: installing
/// the missing library does not change the record's identity, so recording
/// it as missing would pin the host to software until something else did.
fn native_encoder_fact(
    reason: Option<arcen_media::video::BackendUnavailableReason>,
    nvidia_driver_loaded: bool,
) -> CapabilityFact {
    use arcen_media::video::BackendUnavailableReason as Reason;
    match reason {
        Some(Reason::HardwareUnavailable | Reason::NotBuilt) => CapabilityFact::Missing,
        Some(Reason::RuntimeMissing) if !nvidia_driver_loaded => CapabilityFact::Missing,
        _ => CapabilityFact::Unknown,
    }
}

/// What one output's session proved.
#[derive(Debug, Default)]
struct OutputResult {
    /// The headless session started and reported ready.
    started: bool,
    native_encoder: CapabilityFact,
    route: Option<CaptureRouteProof>,
    /// The `READY` backend that encoded the route's proof.
    route_encoder: Option<String>,
    input: Option<InputProof>,
    rates: Vec<SourceRateMeasurement>,
    audio_monitor: Option<bool>,
    /// X11 applications, when the session was started with Xwayland.
    x11: Option<XwaylandProof>,
    teardown_clean: bool,
}

/// What the session's ready message says about its Xwayland, as a record
/// proof.
fn x11_proof(ready: &WaylandAgentReady, version: Option<&str>) -> XwaylandProof {
    let version = version.map(str::to_owned);
    match &ready.x11 {
        None => XwaylandProof {
            display_proven: false,
            window_manager: false,
            pointer: false,
            glamor: GlamorRoute::Unknown,
            xwayland_version: version,
            failure: Some("the session reported nothing about Xwayland".to_owned()),
        },
        Some(x11) => XwaylandProof {
            display_proven: x11.ready
                || x11.failure.as_deref() == Some(super::xwayland::NO_WINDOW_MANAGER),
            window_manager: x11.ready,
            pointer: false,
            glamor: x11.glamor,
            xwayland_version: version,
            failure: x11.failure.clone(),
        },
    }
}

/// Drives the session's pointer over an X11 window and reads where the X
/// server says it is.
async fn x11_pointer(
    ready: &WaylandAgentReady,
    uid: u32,
) -> Result<super::xwayland::PointerProof, String> {
    let x11 = ready.x11.as_ref().ok_or("no X11 state")?;
    let displays = x11.displays.ok_or("no X11 displays")?;
    let auth = x11.xauthority.clone().ok_or("no X authority")?;
    let stream = ready.streams.first().ok_or("no stream")?;
    let target = super::xwayland::PointerTarget {
        input_socket: ready.input_socket.clone(),
        connector: stream.connector.clone(),
        generation: ready.generation,
        rect: (stream.x, stream.y, stream.width, stream.height),
    };
    let runtime_dir = ready.runtime_dir.clone();
    // Supervised: every blocking step has its own socket timeout.
    #[allow(clippy::disallowed_methods)]
    let task = tokio::task::spawn_blocking(move || {
        super::xwayland::prove_pointer(
            &super::xwayland::X11Dirs::production(),
            uid,
            &runtime_dir,
            displays.public,
            &auth,
            &target,
            COMMAND_TIMEOUT,
        )
    });
    task.await.map_err(|error| error.to_string())?
}

/// `Xwayland -version`'s version number, when it runs.
async fn xwayland_version() -> Option<String> {
    let output = command_output(XWAYLAND_BINARY, &["-version"]).await.ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    arcen_session::wayland_x11::parse_xwayland_version(&text)
}

/// Drives absolute pointer motion through the session's input socket at a
/// steady rate and counts what the compositor accepted: every command is
/// answered, so acceptance is the session's own answer.
struct MotionDriver {
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<u64, String>>,
}

impl MotionDriver {
    fn start(ready: &WaylandAgentReady) -> Self {
        let socket = ready.input_socket.clone();
        let stream = ready.streams[0].clone();
        let generation = ready.generation;
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let connection = tokio::net::UnixStream::connect(&socket)
                .await
                .map_err(|error| format!("input socket: {error}"))?;
            let (reader, mut writer) = connection.into_split();
            let mut replies =
                crate::bounded_io::BoundedLineReader::new(tokio::io::BufReader::new(reader));
            let request = |command: InputCommand| {
                let mut line = serde_json::to_vec(&InputRequest::new(command, COMMAND_TIMEOUT))
                    .unwrap_or_default();
                line.push(b'\n');
                line
            };
            // Replies are read concurrently: a writer that waited for each
            // reply would send motion whenever the round trip allowed, two
            // motions could land in one compositor frame, and the content
            // would no longer change at a steady rate.
            let replied = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let accepted = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let reader = tokio::spawn({
                let replied = Arc::clone(&replied);
                let accepted = Arc::clone(&accepted);
                async move {
                    while let Ok(Some(reply)) = replies.next_bounded_line().await {
                        if serde_json::from_str::<InputReply>(&reply.text)
                            .is_ok_and(|reply| reply.ok)
                        {
                            accepted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        replied.fetch_add(1, std::sync::atomic::Ordering::Release);
                    }
                }
            });
            let mut ticker =
                tokio::time::interval(Duration::from_micros(1_000_000 / u64::from(motion_hz())));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let started = Instant::now();
            let (width, height) = (f64::from(stream.width), f64::from(stream.height));
            let mut sent = 0_u64;
            let written = loop {
                tokio::select! {
                    _ = stopped.changed() => break Ok(()),
                    _ = ticker.tick() => {}
                }
                let phase = started.elapsed().as_secs_f64() * std::f64::consts::TAU / 2.0;
                let command = InputCommand::PointerAbsolute {
                    output: stream.connector.clone(),
                    generation,
                    x: (width * (0.5 + 0.35 * phase.cos())).clamp(0.0, width - 1.0),
                    y: (height * (0.5 + 0.35 * phase.sin())).clamp(0.0, height - 1.0),
                };
                if let Err(error) = writer.write_all(&request(command)).await {
                    break Err(format!("input socket write: {error}"));
                }
                sent += 1;
            };
            // Every command sent is answered; bound the wait for the last.
            let deadline = Instant::now() + COMMAND_TIMEOUT;
            while replied.load(std::sync::atomic::Ordering::Acquire) < sent
                && Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            reader.abort();
            written?;
            let accepted = accepted.load(std::sync::atomic::Ordering::Relaxed);
            Ok(accepted)
        });
        Self { stop, task }
    }

    async fn finish(self) -> Result<u64, String> {
        let _ = self.stop.send(true);
        self.task
            .await
            .map_err(|error| format!("motion driver: {error}"))?
    }
}

/// Presses and releases Escape twice through the session's input socket.
async fn leave_overview(ready: &WaylandAgentReady) -> Result<u32, String> {
    const KEY_ESC: u32 = 1;
    let connection = tokio::net::UnixStream::connect(&ready.input_socket)
        .await
        .map_err(|error| format!("input socket: {error}"))?;
    let (reader, mut writer) = connection.into_split();
    let mut replies = crate::bounded_io::BoundedLineReader::new(tokio::io::BufReader::new(reader));
    let mut accepted = 0;
    for _ in 0..2 {
        for pressed in [true, false] {
            let mut line = serde_json::to_vec(&InputRequest::new(
                InputCommand::Key {
                    keycode: KEY_ESC,
                    pressed,
                },
                COMMAND_TIMEOUT,
            ))
            .map_err(|error| error.to_string())?;
            line.push(b'\n');
            writer
                .write_all(&line)
                .await
                .map_err(|error| format!("input socket write: {error}"))?;
            let reply = tokio::time::timeout(COMMAND_TIMEOUT, replies.next_bounded_line())
                .await
                .map_err(|_| "the session did not answer a key".to_owned())?
                .map_err(|error| format!("input socket read: {error}"))?
                .ok_or("the session closed the input socket")?;
            if serde_json::from_str::<InputReply>(&reply.text).is_ok_and(|reply| reply.ok)
                && !pressed
            {
                accepted += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Let the overview's closing animation finish before measuring.
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(accepted)
}

/// The encoder a session on this host is served with.
const fn session_encoder(native_encoder: CapabilityFact) -> EncoderSelection {
    match native_encoder {
        CapabilityFact::Missing => EncoderSelection::SoftwareH264,
        CapabilityFact::Available => EncoderSelection::NativeNvenc,
        // As a session would be served: NVENC first, then OpenH264.
        CapabilityFact::Unknown => EncoderSelection::Auto,
    }
}

/// A running product capture of the session's first output.
struct StreamConsumer {
    session: capenc::CapencSession,
    drain: tokio::task::JoinHandle<()>,
}

impl StreamConsumer {
    async fn finish(self) {
        self.session.shutdown().await;
        let _ = self.drain.await;
    }
}

struct Run<'run> {
    config: &'run crate::cli::Config,
    launcher: &'run SessionLauncher,
    ready: &'run WaylandAgentReady,
    gpu_import: Option<GpuImportVerification>,
    /// Whether a native encoder opened for this session's stream; without
    /// one the session is qualified, as it will be served, in software.
    native_encoder: CapabilityFact,
}

impl Run<'_> {
    fn capenc_config(&self, speed: bool) -> Result<CapencConfig, String> {
        let binary = self
            .config
            .resolve_capenc_binary()
            .ok_or("the Pier executable cannot be resolved for capenc")?;
        let stream = &self.ready.streams[0];
        let mut config = self.config.capenc_config_for_size(
            binary,
            Some(self.launcher.execution()),
            crate::eventlog::random_correlation_id(),
            CursorMode::Host,
            stream.width,
            stream.height,
        );
        let pipeline = if speed {
            arcen_media::video::PipelineId::Speed
        } else {
            arcen_media::video::PipelineId::Auto
        };
        let contract = arcen_media::video::pipeline_contract(pipeline);
        config.codec = "h264".to_owned();
        config.encoder = session_encoder(self.native_encoder);
        config.fps = contract.max_fps;
        config.yuv444 = false;
        config.bit_depth = arcen_media::BitDepth::Eight;
        config.color_range = arcen_media::ColorRange::Limited;
        config.color_matrix = arcen_media::ColorMatrix::Bt709;
        config.transfer = arcen_media::TransferCharacteristics::Bt709;
        config.color_primaries = arcen_media::ColorPrimaries::Bt709;
        config.desktop_encoding = arcen_media::video::DesktopSignalEncoding::Sdr;
        config.video_selection = VideoSelectionIntent::AdaptivePerformance;
        config.codec_pinned = false;
        config.variant_pinned = false;
        config.intent = arcen_media::EncodeIntent::Interactive;
        config.motion_priority = if speed {
            MotionPriority::Motion
        } else {
            MotionPriority::Detail
        };
        config.qp_map = arcen_media::video::QpMapPolicy::Off;
        config.encoder_max_bitrate_bps = contract.encoder_ceiling_bps();
        // No rate from a record: the Speed measurement run sets only the
        // ceiling it measures against (`speed_rate`).
        config.capture_rate = None;
        // The route the product would take: GPU import only once verified.
        config.gpu_import_verified = self
            .gpu_import
            .as_ref()
            .is_some_and(GpuImportVerification::verified);
        Ok(config)
    }

    /// Whether a native encoder opens for this session's stream: capenc's
    /// own `probe-v1` (CUDA and NVENC at the stream's size, no capture).
    async fn probe_native_encoder(&self) -> CapabilityFact {
        let config = match self.capenc_config(false) {
            Ok(config) => config,
            Err(error) => {
                step("native_encoder", false, &error);
                return CapabilityFact::Unknown;
            }
        };
        match capenc::probe_native_encoder(&config).await {
            Ok(None) => {
                step("native_encoder", true, "NVENC opened for the stream");
                CapabilityFact::Available
            }
            Ok(Some(line)) => {
                let reason = arcen_media::video::parse_unavailable_v1(&line)
                    .ok()
                    .map(|notice| notice.reason);
                let fact = native_encoder_fact(reason, qualification::gpu_driver().is_some());
                let unavailable = line.trim_start_matches("[capenc] ");
                // Neither is a failure: a host without a native encoder is
                // qualified for software encode, and one whose NVIDIA stack
                // is merely incomplete keeps trying NVENC first.
                step(
                    "native_encoder",
                    true,
                    &if fact == CapabilityFact::Missing {
                        format!(
                            "no native encoder ({unavailable}); sessions here are served by OpenH264"
                        )
                    } else {
                        format!(
                            "NVENC did not open ({unavailable}); not recorded as missing, \
                             sessions keep trying NVENC first and fall back to OpenH264"
                        )
                    },
                );
                fact
            }
            Err(error) => {
                step("native_encoder", false, &format!("probe: {error}"));
                CapabilityFact::Unknown
            }
        }
    }

    /// The session's capture as a Deck is served it: capenc on the first
    /// output with the encoder the session would get (OpenH264 where no
    /// native encoder opened). Returns after capenc's first access unit,
    /// so the stream is being consumed.
    async fn stream_consumer(&self) -> Result<StreamConsumer, String> {
        let config = self.capenc_config(false)?;
        let (mut session, _plan) = capenc::spawn(config)
            .await
            .map_err(|error| format!("capenc: {error}"))?;
        let mut frames = session.take_frames();
        let drain = tokio::spawn(async move {
            if let Some(frames) = frames.as_mut() {
                while frames.recv().await.is_some() {}
            }
        });
        Ok(StreamConsumer { session, drain })
    }

    /// Verifies capenc's GPU import on the session's still desktop.
    /// Runs capenc's capture probe on this desktop's metadata stream while
    /// the pointer moves, and reads the cursor facts it saw.
    async fn cursor_metadata(&self) -> Result<CursorMetadataProbe, String> {
        let mut config = self.capenc_config(false)?;
        config.cursor_mode = CursorMode::Local;
        let mut command = capenc::wayland_capture_probe_command(
            &config,
            &["frames=2", "timeout-ms=3000", "verify-upload=0"],
        )
        .map_err(|error| format!("capture harness: {error}"))?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let motion = MotionDriver::start(self.ready);
        let output = tokio::time::timeout(GPU_IMPORT_TIMEOUT, command.output()).await;
        let accepted = motion.finish().await;
        let stderr = match output {
            Ok(Ok(output)) => String::from_utf8_lossy(&output.stderr).into_owned(),
            Ok(Err(error)) => return Err(format!("capture harness: {error}")),
            Err(_) => return Err("capture harness timed out".to_owned()),
        };
        tracing::info!(
            target: crate::logging::target::SESSION,
            accepted_pointer_motions = ?accepted,
            "cursor metadata probe finished"
        );
        stderr
            .lines()
            .find_map(parse_cursor_metadata_probe)
            .ok_or_else(|| "the capture probe reported no summary".to_owned())
    }

    async fn verify_gpu_import(&self) -> Option<GpuImportVerification> {
        let config = match self.capenc_config(false) {
            Ok(config) => config,
            Err(error) => {
                step("gpu_import", false, &error);
                return None;
            }
        };
        let mut last = None;
        for attempt in 1..=GPU_IMPORT_ATTEMPTS {
            // Let the desktop settle: both captures must show the same frame.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let mut command = match capenc::wayland_capture_probe_command(
                &config,
                &[
                    "verify-gpu-import",
                    "frames=2",
                    "timeout-ms=4000",
                    "verify-upload=0",
                ],
            ) {
                Ok(command) => command,
                Err(error) => {
                    step("gpu_import", false, &format!("capture harness: {error}"));
                    return None;
                }
            };
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let output = tokio::time::timeout(GPU_IMPORT_TIMEOUT, command.output()).await;
            let stderr = match output {
                Ok(Ok(output)) => String::from_utf8_lossy(&output.stderr).into_owned(),
                Ok(Err(error)) => {
                    step("gpu_import", false, &format!("capture harness: {error}"));
                    return None;
                }
                Err(_) => {
                    step("gpu_import", false, "capture harness timed out");
                    return None;
                }
            };
            let route = stderr
                .lines()
                .find(|line| {
                    line.contains("wayland-probe gpu-convert ") && line.contains("render_node=")
                })
                .or_else(|| {
                    stderr
                        .lines()
                        .find(|line| line.contains("wayland-probe gpu-convert unavailable"))
                })
                .map(|line| line.trim_start_matches("[capenc] ").to_owned());
            let verification = stderr.lines().find_map(parse_gpu_import_verify);
            let verified = verification
                .as_ref()
                .is_some_and(GpuImportVerification::verified);
            let retry = !verified
                && verification
                    .as_ref()
                    .is_some_and(|result| result.result == "MISMATCH")
                && attempt < GPU_IMPORT_ATTEMPTS;
            if !retry {
                step(
                    "gpu_import",
                    verified,
                    &format!(
                        "attempt {attempt}: {}; {}",
                        verification.as_ref().map_or_else(
                            || "no verification reported".to_owned(),
                            |result| format!(
                                "result={} gpu_frames={} pixels={} mismatched={} sync={}",
                                result.result,
                                result.gpu_frames,
                                result.pixels,
                                result.mismatched,
                                result.sync
                            )
                        ),
                        route.as_deref().unwrap_or("importer state not reported"),
                    ),
                );
                return verification;
            }
            last = verification;
        }
        last
    }

    /// Auto: the first frame proves the capture route; the encoder must
    /// keep producing while the pointer moves.
    async fn auto(
        &self,
        readiness: &mut WaylandSessionReadiness,
    ) -> Option<(CaptureRouteProof, String)> {
        let config = match self.capenc_config(false) {
            Ok(config) => config,
            Err(error) => {
                step("capture_route", false, &error);
                return None;
            }
        };
        let (mut session, plan) = match capenc::spawn(config).await {
            Ok(spawned) => spawned,
            Err(error) => {
                step("capture_route", false, &format!("capenc: {error}"));
                return None;
            }
        };
        let Some(notice) = session.capture_admission().copied() else {
            step("capture_route", false, "capenc READY without an admission");
            session.shutdown().await;
            return None;
        };
        let route = match notice.copy_path {
            CopyPath::HostCopy => Some(CaptureRoute::HostCopy),
            CopyPath::GpuConvert => Some(CaptureRoute::GpuConvert),
            CopyPath::ZeroCopyImport => Some(CaptureRoute::ZeroCopyImport),
            CopyPath::CrossGpu => None,
        };
        let frame = readiness.record_frame(FrameEvidence {
            node_id: notice.frame.node_id,
            format: notice.frame.format.token().to_owned(),
            width: notice.frame.width,
            height: notice.frame.height,
            memory: match notice.frame.memory {
                CaptureMemory::HostShared => FrameMemory::SharedMemory,
                CaptureMemory::DmaBuf { .. } => FrameMemory::DmaBuf,
                CaptureMemory::GpuDevice => FrameMemory::Unknown,
            },
            import: match route {
                Some(CaptureRoute::HostCopy) => FrameImport::HostCopy,
                Some(CaptureRoute::GpuConvert) => FrameImport::GpuConvert,
                Some(CaptureRoute::ZeroCopyImport) => FrameImport::ZeroCopyImport,
                None => FrameImport::Unreported,
            },
        });
        let mut frames = session.take_frames();
        let motion = MotionDriver::start(self.ready);
        let mut access_units = 0_u32;
        if let Some(frames) = frames.as_mut() {
            let deadline = tokio::time::Instant::now() + AUTO_RUN;
            while let Ok(Some(_)) = tokio::time::timeout_at(deadline, frames.recv()).await {
                access_units += 1;
            }
        }
        let accepted = motion.finish().await.unwrap_or(0);
        session.shutdown().await;
        let ok = frame.is_ok() && route.is_some() && access_units > 0;
        step(
            "capture_route",
            ok,
            &format!(
                "admission {} served {:?} at {:?} fps; first frame node={} {} {}x{} {}; route {}; \
                 {access_units} access units in {}s with {accepted} pointer motions; plan {} {}x{}@{} \
                 cursor={:?}",
                notice.admission.decision_token(),
                notice.served().map(|served| served.pipeline.token()),
                notice.served().map(|served| served.fps_ceiling),
                notice.frame.node_id,
                notice.frame.format.token(),
                notice.frame.width,
                notice.frame.height,
                notice.frame.memory.token(),
                notice.copy_path.token(),
                AUTO_RUN.as_secs(),
                plan.codec_token(),
                plan.width,
                plan.height,
                plan.fps,
                plan.cursor_mode,
            ),
        );
        let route = route.unwrap_or(CaptureRoute::HostCopy);
        let verified_frames = match (&self.gpu_import, route.gpu_import()) {
            (Some(verification), true) if verification.verified() => {
                u32::try_from(verification.gpu_frames).unwrap_or(u32::MAX)
            }
            _ => 0,
        };
        ok.then(|| {
            (
                CaptureRouteProof {
                    route,
                    format: notice.frame.format.token().to_owned(),
                    memory: notice.frame.memory.token().to_owned(),
                    width: notice.frame.width,
                    height: notice.frame.height,
                    encoded_access_units: access_units,
                    gpu_import_verified_frames: verified_frames,
                },
                plan.backend.ready_token().to_owned(),
            )
        })
    }

    /// Speed: the request offers the compositor twice Speed's ceiling and is
    /// admitted at the ceiling; the rate capenc reports both delivered by
    /// the source and encoded while the pointer moves at Speed's rate is the
    /// measurement.
    async fn speed_rate(
        &self,
        display_refresh_hz: u32,
        codec: &str,
        seconds: u32,
    ) -> SourceRateMeasurement {
        let stream = &self.ready.streams[0];
        let gpu_route = self
            .gpu_import
            .as_ref()
            .is_some_and(GpuImportVerification::verified);
        let mut measurement = SourceRateMeasurement {
            width: stream.width,
            height: stream.height,
            display_refresh_hz,
            refresh_hz: self.ready.outputs[0].refresh_hz,
            codec: codec.to_owned(),
            route: if gpu_route {
                CaptureRoute::GpuConvert
            } else {
                CaptureRoute::HostCopy
            },
            sustained_source_fps: None,
            seconds,
            evidence: RateEvidence::Measured,
        };
        let refresh_hz = measurement.refresh_hz;
        let mut config = match self.capenc_config(true) {
            Ok(config) => config,
            Err(error) => {
                step("speed_rate", false, &error);
                return measurement;
            }
        };
        // Admitted at Speed's ceiling so the encoder runs at Speed's rate
        // while it is measured; what is recorded is only what this run both
        // delivered and encoded, which the assertion cannot raise.
        config.capture_rate = Some(pipeline_contract(PipelineId::Speed).max_fps);
        codec.clone_into(&mut config.codec);
        let (mut session, _plan) = match capenc::spawn(config).await {
            Ok(spawned) => spawned,
            Err(error) => {
                step("speed_rate", false, &format!("capenc: {error}"));
                return measurement;
            }
        };
        let decision = session.capture_admission().map(|notice| {
            format!(
                "{} served {:?} ({:?})",
                notice.admission.decision_token(),
                notice.served().map(|served| served.pipeline.token()),
                match &notice.admission {
                    arcen_media::video::PresetAdmission::Downgrade { reason, .. }
                    | arcen_media::video::PresetAdmission::Unavailable { reason, .. } => {
                        Some(reason.token())
                    }
                    arcen_media::video::PresetAdmission::Admit(_) => None,
                },
            )
        });
        let mut frames = session.take_frames();
        let drain = tokio::spawn(async move {
            let mut count = 0_u64;
            if let Some(frames) = frames.as_mut() {
                while frames.recv().await.is_some() {
                    count += 1;
                }
            }
            count
        });
        let mut stats = session.pipeline_stats();
        let motion = MotionDriver::start(self.ready);
        let start = std::time::Instant::now();
        let mut meter = SustainedRateMeter::new(start);
        let mut samples = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(seconds));
        while let Ok(Ok(())) = tokio::time::timeout_at(deadline, stats.changed()).await {
            let Some(sample) = *stats.borrow_and_update() else {
                continue;
            };
            if let Some(source) = sample.source {
                // One stats line per second: sample n fills window n. The
                // rate counted is what was both delivered and encoded: a
                // source the encoder cannot keep up with does not serve it.
                let at = start + Duration::from_secs(samples.len() as u64);
                let served = source.min(sample.encoded);
                meter.record(at, served);
                samples.push((source, sample.encoded));
            }
        }
        meter.tick(start + Duration::from_secs(samples.len() as u64));
        let accepted = motion.finish().await.unwrap_or(0);
        session.shutdown().await;
        let access_units = drain.await.unwrap_or(0);
        measurement.sustained_source_fps = meter.best_sustained();
        step(
            "speed_rate",
            measurement.sustained_source_fps.is_some(),
            &format!(
                "{}x{}@{refresh_hz} (display @{display_refresh_hz}) {codec} {}: admission {}; (source, \
                 encoded) fps per second {samples:?}; sustained {:?}; {access_units} access units; \
                 {accepted} pointer motions",
                measurement.width,
                measurement.height,
                measurement.route.token(),
                decision.as_deref().unwrap_or("none"),
                measurement.sustained_source_fps,
            ),
        );
        measurement
    }

    async fn audio_monitor(&self) -> bool {
        let mut pactl = Command::new(PACTL);
        pactl
            .args(["list", "short", "sources"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if self.launcher.execution().configure(&mut pactl).is_err() {
            step(
                "audio_monitor",
                false,
                "cannot run pactl as the session user",
            );
            return false;
        }
        let sources = tokio::time::timeout(Duration::from_secs(10), pactl.output())
            .await
            .ok()
            .and_then(Result::ok)
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        // The session's own capture source when its agent named one (a
        // private graph's session sink, or a local-playback lease's), any
        // monitor otherwise.
        let wanted = self
            .launcher
            .audio
            .as_ref()
            .and_then(|audio| audio.capture_source.clone());
        let monitor = sources
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .find(|name| match wanted.as_deref() {
                Some(wanted) => *name == wanted,
                None => name.ends_with(".monitor"),
            })
            .map(str::to_owned);
        step(
            "audio_monitor",
            monitor.is_some(),
            &format!(
                "PulseAudio at PULSE_RUNTIME_PATH={} mode={} native_pipewire={} monitor={monitor:?}",
                self.launcher
                    .environment
                    .get("PULSE_RUNTIME_PATH")
                    .unwrap_or_default(),
                self.launcher
                    .audio
                    .as_ref()
                    .map_or("unknown", |audio| audio.mode.token()),
                self.launcher
                    .audio
                    .as_ref()
                    .is_some_and(|audio| audio.native_pipewire),
            ),
        );
        monitor.is_some()
    }
}

fn record_session_side(
    ready: &WaylandAgentReady,
    readiness: &mut WaylandSessionReadiness,
) -> Result<(), String> {
    let compositor = if ready.compositor == HeadlessCompositor::GnomeShell.token() {
        HeadlessCompositor::GnomeShell
    } else {
        HeadlessCompositor::Mutter
    };
    readiness
        .record_compositor(compositor)
        .map_err(|error| error.to_string())?;
    readiness
        .record_outputs(
            ready
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
        .map_err(|error| error.to_string())?;
    for stream in &ready.streams {
        readiness
            .record_stream(ScreenCastStream {
                connector: stream.connector.clone(),
                node_id: stream.node_id,
                width: stream.width,
                height: stream.height,
            })
            .map_err(|error| error.to_string())?;
    }
    readiness
        .record_input(RemoteDesktopEvidence {
            device_types: ready.input_device_types,
            started: true,
            linked_to_screencast: true,
            absolute_pointer_accepted: true,
        })
        .map_err(|error| error.to_string())
}

async fn session_gone(session_id: &str, runtime_dir: &Path) -> bool {
    let deadline = Instant::now() + TEARDOWN_WAIT;
    loop {
        let session = command_output(LOGINCTL, &["show-session", session_id, "-p", "State"])
            .await
            .is_ok_and(|output| output.status.success());
        if !session && !runtime_dir.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The launcher configuration a qualification desktop is opened with.
fn qualification_launcher_config(
    config: &crate::cli::Config,
) -> Result<LauncherConfig, &'static str> {
    Ok(LauncherConfig {
        binary: config
            .resolve_session_launcher_binary()
            .ok_or("the session launcher cannot be resolved")?,
        pam_service: config.pam_service.clone(),
        display: config.session_display.clone(),
        gpu_head: config.session_gpu_head.clone(),
        xorg_binary: config.xorg_bin.clone(),
        xorg_config_template: config.xorg_config_template.clone(),
        runtime_root: config.session_runtime_root.clone(),
        agent_binary: config
            .resolve_session_agent_binary()
            .ok_or("the session agent cannot be resolved")?,
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
        // Only the X11 qualification session asks for Xwayland.
        x11_apps: false,
    })
}

/// What a probe of a metadata cursor-mode stream reported: the summary's
/// `cursor_metadata` and `cursor_bitmap` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorMetadataProbe {
    metadata_seen: bool,
    bitmap: Option<String>,
}

impl CursorMetadataProbe {
    /// The fact: cursor metadata with a sprite bitmap.
    fn fact(probe: Option<&Self>) -> CapabilityFact {
        match probe {
            Some(probe) if probe.metadata_seen && probe.bitmap.is_some() => {
                CapabilityFact::Available
            }
            Some(_) => CapabilityFact::Missing,
            None => CapabilityFact::Unknown,
        }
    }
}

/// Parses capenc's `wayland-probe summary …` line.
fn parse_cursor_metadata_probe(line: &str) -> Option<CursorMetadataProbe> {
    let rest = line.split_once("wayland-probe summary ")?.1;
    let field = |name: &str| {
        rest.split_whitespace()
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    };
    Some(CursorMetadataProbe {
        metadata_seen: field("cursor_metadata")? == "true",
        bitmap: field("cursor_bitmap")
            .filter(|bitmap| *bitmap != "-")
            .map(str::to_owned),
    })
}

/// Proves cursor metadata on its own short desktop, recorded with
/// `cursor-mode` metadata: a Deck-drawn cursor needs a sprite bitmap that
/// changes with the pointer. The Speed measurement keeps its embedded
/// cursor on the main desktop, where pointer motion produces frames.
async fn qualify_cursor_metadata(
    config: &crate::cli::Config,
    user: &QualificationUser,
    QualifyOutput {
        width,
        height,
        display_refresh_hz,
    }: QualifyOutput,
) -> Option<CursorMetadataProbe> {
    let launcher_config = match qualification_launcher_config(config) {
        Ok(launcher_config) => launcher_config,
        Err(error) => {
            step("cursor_metadata", false, error);
            return None;
        }
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
            step("cursor_metadata", false, &format!("authenticate: {error}"));
            return None;
        }
    };
    pending.set_session_display(Some(crate::session::launcher::SessionDisplayMsg::served(
        &ClientMonitor {
            refresh_hz: display_refresh_hz,
            ..ClientMonitor::default()
        },
        width,
        height,
    )));
    pending.set_stream_cursor_mode(super::StreamCursorMode::Metadata);
    let launcher = match pending.open().await {
        Ok(launcher) => launcher,
        Err(error) => {
            step(
                "cursor_metadata",
                false,
                &format!("metadata cursor desktop did not start: {error}"),
            );
            return None;
        }
    };
    let session_id = launcher
        .environment
        .session_id()
        .unwrap_or_default()
        .to_owned();
    let probe = match launcher.wayland.clone() {
        Some(ready) if ready.cursor_mode == super::StreamCursorMode::Metadata => {
            let run = Run {
                config,
                launcher: &launcher,
                ready: &ready,
                gpu_import: None,
                // A capture probe: it encodes nothing.
                native_encoder: CapabilityFact::Unknown,
            };
            run.cursor_metadata().await
        }
        Some(_) => Err("the desktop did not record its cursor as metadata".to_owned()),
        None => Err("the launcher started no Wayland session".to_owned()),
    };
    let runtime_dir = launcher
        .wayland
        .as_ref()
        .map(|ready| ready.runtime_dir.clone());
    let stopped = launcher.shutdown().await;
    let gone = match &runtime_dir {
        Some(runtime_dir) => session_gone(&session_id, runtime_dir).await,
        None => true,
    };
    let probe = match probe {
        Ok(probe) => probe,
        Err(error) => {
            step("cursor_metadata", false, &error);
            return None;
        }
    };
    step(
        "cursor_metadata",
        CursorMetadataProbe::fact(Some(&probe)) == CapabilityFact::Available,
        &format!(
            "metadata={} sprite bitmap {}; desktop stopped={stopped} gone={gone}",
            probe.metadata_seen,
            probe.bitmap.as_deref().unwrap_or("none"),
        ),
    );
    Some(probe)
}

/// How long the native-seat proof waits for the compositor to announce the
/// desktop's tablet and its pen.
const NATIVE_SEAT_PROOF_BUDGET: Duration = Duration::from_secs(15);

/// A desktop on its own logind seat with a real pen
/// ([`arcen_session::native_seat`]): the host facts first, then one native
/// desktop whose compositor must announce the desktop's tablet, and the
/// pen with pressure and tilt, to a Wayland client. `None` only when the
/// proof could not be judged at all; a host that cannot have a native seat
/// records why, and its desktops stay headless.
async fn qualify_native_seat(
    config: &crate::cli::Config,
    user: &QualificationUser,
    QualifyOutput {
        width,
        height,
        display_refresh_hz,
    }: QualifyOutput,
) -> Option<arcen_session::native_seat::NativeSeatProof> {
    use arcen_session::native_seat::{
        DesktopSeatPlan, NativeSeatProof, NativeSeatUnavailable, SessionMode,
    };
    if config.wayland_session_mode == SessionMode::Headless {
        step(
            "native_seat",
            true,
            "not tried (wayland.session_mode = headless)",
        );
        return Some(NativeSeatProof::unavailable(
            NativeSeatUnavailable::Configured,
            false,
        ));
    }
    #[allow(
        clippy::disallowed_methods,
        reason = "awaited to completion, never under a timeout; it reads sysfs and runs short host queries"
    )]
    let facts = tokio::task::spawn_blocking(crate::input::seat_pen::native_seat_host_facts)
        .await
        .unwrap_or_default();
    if let Some(reason) = facts.blocker() {
        step(
            "native_seat",
            true,
            &format!(
                "not offered on this host ({}): {}; desktops stay headless",
                reason.token(),
                reason.explanation()
            ),
        );
        return Some(NativeSeatProof::unavailable(
            reason,
            facts.display_manager_active,
        ));
    }
    let launcher_config = match qualification_launcher_config(config) {
        Ok(launcher_config) => launcher_config,
        Err(error) => {
            step("native_seat", false, error);
            return None;
        }
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
            step("native_seat", false, &format!("authenticate: {error}"));
            return None;
        }
    };
    pending.set_session_display(Some(crate::session::launcher::SessionDisplayMsg::served(
        &ClientMonitor {
            refresh_hz: display_refresh_hz,
            ..ClientMonitor::default()
        },
        width,
        height,
    )));
    pending.set_desktop_seat_plan(DesktopSeatPlan::Native { required: true });
    let started = Instant::now();
    let launcher = match pending.open().await {
        Ok(launcher) => launcher,
        Err(error) => {
            let reason = match error {
                crate::session::launcher::LauncherError::NativeSeatUnavailable(reason) => reason,
                crate::session::launcher::LauncherError::PolkitUnprotected(reason) => reason.into(),
                _ => NativeSeatUnavailable::SeatSetupFailed,
            };
            step(
                "native_seat",
                false,
                &format!("native desktop did not start ({}): {error}", reason.token()),
            );
            return Some(NativeSeatProof::unavailable(
                reason,
                facts.display_manager_active,
            ));
        }
    };
    let session_id = launcher
        .environment
        .session_id()
        .unwrap_or_default()
        .to_owned();
    // The launcher admitted the desktop only after the shared polkit
    // admission refused its session (on its Arcen seat) the local grants;
    // asked again here, by the same path, for the record.
    let agent_pid = launcher.agent_pid;
    let polkit_session = session_id.clone();
    #[allow(
        clippy::disallowed_methods,
        reason = "polkit tools are bounded by their own timeout (polkit_remote::TOOL_TIMEOUT)"
    )]
    let (polkit, polkit_checks) = tokio::task::spawn_blocking(move || {
        crate::session::polkit_remote::prove_desktop_checks(
            agent_pid,
            &polkit_session,
            arcen_session::polkit_remote::DesktopSeat::ArcenSeat,
            crate::session::polkit_remote::state(),
        )
    })
    .await
    .unwrap_or((
        arcen_session::polkit_remote::Admission::Refuse(
            arcen_session::polkit_remote::PolkitUnprotected::Unverified,
        ),
        Vec::new(),
    ));
    step(
        "native_seat_polkit",
        polkit == arcen_session::polkit_remote::Admission::Admit,
        &format!(
            "{} admission {polkit:?}; pkcheck for the native session (0 = authorized, 1 = no, 2 = needs admin): {}",
            arcen_session::polkit_remote::RULE_FILE_NAME,
            polkit_checks
                .iter()
                .map(|check| format!("{}={}", check.action, check.status))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    );
    let proof = match (launcher.wayland.clone(), launcher.seat_pen.clone()) {
        (Some(ready), Some(pen)) if pen.is_native() => {
            // GNOME Shell starts in the overview, whose grab holds tablet
            // events back from clients.
            if let Err(error) = leave_overview(&ready).await {
                step("native_seat", true, &format!("overview not left: {error}"));
            }
            let socket = ready.runtime_dir.join(&ready.wayland_display);
            let name = pen.device_name();
            let pen_name = name.clone();
            #[allow(
                clippy::disallowed_methods,
                reason = "awaited to completion; the client reads with a socket timeout until NATIVE_SEAT_PROOF_BUDGET ends"
            )]
            let observed = tokio::task::spawn_blocking(move || {
                super::tablet_proof::observe(&socket, &name, NATIVE_SEAT_PROOF_BUDGET, || {
                    if let Err(error) = pen.bring_into_proximity() {
                        tracing::warn!(target: crate::logging::target::SESSION, %error, "the qualification pen did not come into proximity");
                    }
                })
            })
            .await;
            match observed {
                Ok(Ok(observation)) => Ok((
                    NativeSeatProof::from_observation(
                        &observation,
                        &pen_name,
                        polkit,
                        facts.gpu_id_path.clone(),
                        facts.display_manager_active,
                    ),
                    observation,
                )),
                Ok(Err(error)) => Err(format!("tablet client: {error}")),
                Err(error) => Err(format!("tablet client: {error}")),
            }
        }
        (Some(_), _) => Err("the desktop started without its own seat".to_owned()),
        (None, _) => Err("the launcher started no Wayland session".to_owned()),
    };
    let runtime_dir = launcher
        .wayland
        .as_ref()
        .map(|ready| ready.runtime_dir.clone());
    let stopped = launcher.shutdown().await;
    let gone = match &runtime_dir {
        Some(runtime_dir) => session_gone(&session_id, runtime_dir).await,
        None => true,
    };
    match proof {
        Ok((proof, observation)) => {
            step(
                "native_seat",
                proof.proven(),
                &format!(
                    "desktop on its own seat ready in {} ms; polkit kept it remote {}; tablet {} pen pressure {} tilt {} \
                     (tablets {:?}, tool types {:?}, capabilities {:?}); display manager active \
                     {}; desktop stopped={stopped} gone={gone}",
                    started.elapsed().as_millis(),
                    proof.polkit_effective,
                    proof.tablet,
                    proof.pressure,
                    proof.tilt,
                    observation.tablets,
                    observation.tool_types,
                    observation.tool_capabilities,
                    proof.display_manager_active,
                ),
            );
            Some(proof)
        }
        Err(error) => {
            step(
                "native_seat",
                false,
                &format!("{error}; desktop stopped={stopped} gone={gone}"),
            );
            Some(NativeSeatProof::unavailable(
                NativeSeatUnavailable::QualificationFailed,
                facts.display_manager_active,
            ))
        }
    }
}

/// One headless session for `output` with its output at `refresh_hz`:
/// the route, input and (when `check_audio`) audio proofs, then Speed's rate
/// with each of `codecs`.
async fn qualify_output(
    config: &crate::cli::Config,
    user: &QualificationUser,
    output: QualifyOutput,
    refresh_hz: u32,
    codecs: &[String],
    seconds: u32,
    check_audio: bool,
    x11: Option<&Option<String>>,
) -> OutputResult {
    let QualifyOutput { width, height, .. } = output;
    // Nothing opened yet is nothing left behind: a display a session cannot
    // be opened for records no rate and does not fail the others.
    let mut result = OutputResult {
        teardown_clean: true,
        ..OutputResult::default()
    };
    let launcher_config = match qualification_launcher_config(config) {
        Ok(launcher_config) => LauncherConfig {
            x11_apps: x11.is_some(),
            ..launcher_config
        },
        Err(error) => {
            step("session", false, error);
            return result;
        }
    };
    let Ok(permit) = Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
    else {
        return result;
    };
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
            step(
                "authenticate",
                false,
                &format!("PAM service {}: {error}", config.pam_service),
            );
            return result;
        }
    };
    pending.set_session_display(Some(crate::session::launcher::SessionDisplayMsg::served(
        &ClientMonitor {
            refresh_hz,
            ..ClientMonitor::default()
        },
        width,
        height,
    )));
    let started = Instant::now();
    let launcher = match pending.open().await {
        Ok(launcher) => launcher,
        Err(error) => {
            step(
                "session",
                false,
                &format!("headless session did not start: {error}"),
            );
            result.teardown_clean = !has_sessions(&user.name).await;
            return result;
        }
    };
    let session_id = launcher
        .environment
        .session_id()
        .unwrap_or_default()
        .to_owned();
    let Some(ready) = launcher.wayland.clone() else {
        step("session", false, "the launcher started no Wayland session");
        result.teardown_clean = launcher.shutdown().await;
        return result;
    };
    result.started = true;
    if let Some(version) = x11 {
        let proof = x11_proof(&ready, version.as_deref());
        step(
            "x11_apps",
            proof.display_proven && proof.window_manager,
            &format!(
                "display={} setup_display={} glamor={} Xwayland {}; {}",
                ready
                    .x11
                    .as_ref()
                    .and_then(super::ReadyX11::display)
                    .unwrap_or_else(|| "none".to_owned()),
                ready
                    .x11
                    .as_ref()
                    .and_then(super::ReadyX11::setup_display)
                    .unwrap_or_else(|| "none".to_owned()),
                proof.glamor.token(),
                proof.xwayland_version.as_deref().unwrap_or("unknown"),
                proof
                    .failure
                    .as_deref()
                    .unwrap_or("a client with the session cookie was served on a managed screen",),
            ),
        );
        result.x11 = Some(proof);
    }
    let mut readiness = WaylandSessionReadiness::new();
    let session_side = record_session_side(&ready, &mut readiness);
    step(
        "session",
        session_side.is_ok(),
        &format!(
            "{} {}x{}@{} logind session {session_id} ready in {} ms; node {}; {session_side:?}",
            ready.compositor,
            ready.outputs[0].width,
            ready.outputs[0].height,
            ready.outputs[0].refresh_hz,
            started.elapsed().as_millis(),
            ready.streams[0].node_id,
        ),
    );
    if session_side.is_ok() {
        // The Pier's own injector proves the socket, its peer and `hello`.
        let input = match WaylandInputTarget::from_session(
            &ready,
            launcher.identity.uid,
            &session_id,
            launcher.agent_pid,
        ) {
            Ok(target) => match WaylandInjector::connect(target).await {
                Ok((injector, _stats)) => {
                    let proof = injector.proof().clone();
                    drop(injector);
                    Ok(proof)
                }
                Err(error) => Err(error.to_string()),
            },
            Err(error) => Err(error.to_string()),
        };
        let mut run = Run {
            config,
            launcher: &launcher,
            ready: &ready,
            gpu_import: None,
            native_encoder: CapabilityFact::Unknown,
        };
        run.native_encoder = run.probe_native_encoder().await;
        result.native_encoder = run.native_encoder;
        if run.native_encoder == CapabilityFact::Missing {
            // GPU import feeds a native encoder; there is none to feed.
            step(
                "gpu_import",
                true,
                "skipped: no native encoder, so no GPU import route serves this host",
            );
        } else {
            run.gpu_import = run.verify_gpu_import().await;
        }
        if let Some((route, encoder)) = run.auto(&mut readiness).await {
            result.route = Some(route);
            result.route_encoder = Some(encoder);
        }
        // Accepted motion is the session's own answer, on a connection that
        // supersedes the proof's.
        let motion = MotionDriver::start(&ready);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let accepted = motion.finish().await;
        match input {
            Ok(proof) => {
                let capabilities = proof.capabilities();
                let accepted_pointer_motions = accepted.as_ref().copied().unwrap_or(0);
                let input = InputProof {
                    backend: "remote_desktop_dbus".to_owned(),
                    keyboard: capabilities.keyboard,
                    pointer: capabilities.pointer,
                    touch: capabilities.touch,
                    // Pen is the session's emulated stylus, when its
                    // compositor's EIS seat offered one this Pier can bind.
                    pen: if proof.pen_capabilities().pen_available() {
                        CapabilityFact::Available
                    } else {
                        capabilities.pen
                    },
                    eis: capabilities.eis,
                    accepted_pointer_motions,
                };
                step(
                    "input_injection",
                    input.injection() == CapabilityFact::Available,
                    &format!(
                        "hello generation={} device_types={} keyboard={:?} pointer={:?} \
                         touch={:?} pen={:?} eis={:?} eis_offered={:?} stylus={}; {accepted:?} \
                         pointer motions accepted in 1s",
                        proof.facts().generation,
                        proof.facts().device_types,
                        input.keyboard,
                        input.pointer,
                        input.touch,
                        input.pen,
                        input.eis,
                        proof.facts().eis_offered,
                        proof.facts().stylus.as_ref().map_or_else(
                            || "not probed".to_owned(),
                            |stylus| format!(
                                "{:?} ei_stylus={:?} seat={:?}",
                                stylus.unavailable,
                                stylus.ei_stylus_version,
                                stylus.seat_interfaces
                            )
                        ),
                    ),
                );
                result.input = Some(input);
            }
            Err(error) => step("input_injection", false, &error),
        }
        // Speed is measured on the desktop a Deck user works on, not over
        // the Activities overview (and first-login welcome) GNOME Shell
        // opens with, which a Deck user leaves with Escape too.
        let escapes = leave_overview(&ready).await;
        step(
            "leave_overview",
            escapes.is_ok(),
            &escapes.map_or_else(
                |error| error,
                |count| format!("{count} Escape key presses accepted"),
            ),
        );
        // The X11 pointer is proven on the desktop, not over the Activities
        // overview, which keeps the pointer for GNOME Shell.
        if let Some(proof) = result
            .x11
            .as_mut()
            .filter(|proof| proof.display_proven && proof.window_manager)
        {
            // Mutter drops absolute motion aimed at a remote monitor whose
            // stream nobody is consuming, as it is between Deck
            // connections; a Deck's capture is what makes the pointer move.
            let pointer = match run.stream_consumer().await {
                Ok(consumer) => {
                    let pointer = x11_pointer(&ready, launcher.identity.uid).await;
                    consumer.finish().await;
                    pointer
                }
                Err(error) => Err(format!("the session's capture did not start: {error}")),
            };
            match pointer {
                Ok(pointer) => {
                    proof.pointer = pointer.followed();
                    if !proof.pointer {
                        proof.failure = Some(format!(
                            "pointer motion did not reach X11 windows: aimed at {:?}, the X \
                             server's pointer was at {:?}",
                            pointer.expected, pointer.observed
                        ));
                    }
                    step(
                        "x11_pointer",
                        proof.pointer,
                        &format!(
                            "aimed at {:?}; the X server's pointer was at {:?}",
                            pointer.expected, pointer.observed
                        ),
                    );
                }
                Err(error) => {
                    proof.failure = Some(format!("the X11 pointer could not be proven: {error}"));
                    step("x11_pointer", false, &error);
                }
            }
        }
        if run.native_encoder == CapabilityFact::Missing {
            // The software contract serves at most 30 fps: Speed is always
            // visibly downgraded here, so no source rate is worth recording.
            step(
                "speed_rate",
                true,
                "skipped: software encode serves Speed as Software at up to 30 fps",
            );
        } else {
            for codec in codecs {
                result.rates.push(
                    run.speed_rate(output.display_refresh_hz, codec, seconds)
                        .await,
                );
            }
        }
        if check_audio {
            result.audio_monitor = Some(run.audio_monitor().await);
        }
    }
    let runtime_dir = ready.runtime_dir.clone();
    let uid = launcher.identity.uid;
    let stopped = launcher.shutdown().await;
    let gone = stopped && session_gone(&session_id, &runtime_dir).await;
    let x11_left = ready
        .x11
        .as_ref()
        .and_then(|x11| x11.displays)
        .map(|displays| {
            super::xwayland::session_artifacts_present(
                &super::xwayland::X11Dirs::production(),
                uid,
                &displays,
            )
        })
        .unwrap_or_default();
    result.teardown_clean = gone && x11_left.is_empty();
    step(
        "teardown",
        result.teardown_clean,
        &format!(
            "agent stopped={stopped}; logind session {session_id} and {} gone={gone}; X11 sockets and locks left: {x11_left:?}",
            runtime_dir.display(),
        ),
    );
    result
}

async fn pier_service_active() -> bool {
    command_output(SYSTEMCTL, &["is-active", "--quiet", PIER_SERVICE])
        .await
        .is_ok_and(|output| output.status.success())
}

fn load_config(options: &QualifyOptions) -> Result<crate::cli::Config, String> {
    let args = match &options.config {
        Some(path) => vec!["--config".to_owned(), path.display().to_string()],
        // Qualification never listens: the listener's TLS material is
        // irrelevant, but the parser requires it to be named.
        None => [
            "--no-config",
            "--tls-cert",
            "/dev/null",
            "--tls-key",
            "/dev/null",
        ]
        .map(str::to_owned)
        .to_vec(),
    };
    let config = crate::cli::parse(&args)?;
    super::apply_configured_compositor_prefix(config.wayland_compositor_prefix.as_deref())?;
    Ok(config)
}

async fn run(options: QualifyOptions, target: QualificationTarget) -> Result<bool, String> {
    let config = load_config(&options)?;
    run_with(options, target, config, Mode::Command, None).await
}

#[allow(clippy::too_many_lines)]
/// `stop` is the service's stop request (an automatic run); a command run
/// stops on Ctrl-C. Either way the session in progress is abandoned and the
/// temporary account removed before returning.
async fn run_with(
    options: QualifyOptions,
    target: QualificationTarget,
    config: crate::cli::Config,
    mode: Mode,
    stop: Option<tokio::sync::watch::Receiver<crate::service_signals::StopRequest>>,
) -> Result<bool, String> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err("run as root".to_owned());
    }
    // The automatic run is the service itself, starting.
    let service_active = mode == Mode::Command && pier_service_active().await;
    target
        .check_service(PIER_SERVICE, service_active, options.allow_running_service)
        .map_err(|error| error.to_string())?;
    if !target.is_production() {
        step(
            "record",
            true,
            &format!(
                "{} (not the production record; temporary account {}{})",
                target.record_path(),
                target.temporary_user(),
                if service_active {
                    format!(", beside the running {PIER_SERVICE}")
                } else {
                    String::new()
                }
            ),
        );
    }
    let probe = super::facts::probe_host().await;
    let live = probe.facts();
    let compositor = probe
        .compositor()
        .ok_or("no installed compositor runs headless with virtual monitors")?;
    let missing = [
        ("session_service", live.session_service),
        ("pipewire", live.pipewire),
    ]
    .into_iter()
    .filter(|(_, fact)| *fact != CapabilityFact::Available)
    .map(|(name, _)| name)
    .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!("host facts missing: {missing:?}"));
    }
    let identity: HostIdentity = qualification::host_identity(compositor).await?;
    let os = qualification::os_release();
    step(
        "host",
        true,
        &format!(
            "{} {} kernel {}; {} {}; PipeWire {}; GPU driver {}; render node {:?}; Pier {} ({}, \
             sha256 {})",
            os.id,
            os.version_id,
            os.kernel,
            compositor.token(),
            identity.stack.compositor_version,
            identity.stack.pipewire_version,
            identity
                .stack
                .gpu_driver
                .as_ref()
                .map_or_else(|| "none".to_owned(), ToString::to_string),
            live.render_node_access,
            identity.pier.version,
            identity.pier.revision,
            &identity.pier.executable_sha256[..12.min(identity.pier.executable_sha256.len())],
        ),
    );

    let previous = if options.replace_outputs {
        Vec::new()
    } else {
        qualification::previous_outputs(Path::new(target.record_path()))
    };
    let requested = if options.replace_outputs {
        arcen_session::wayland_qualification::RequestedSpeedOutputs::new()
    } else {
        qualification::load_requested_outputs(Path::new(&target.requested_outputs_path()))
    };
    let ladder: &[QualifyOutput] = if options.replace_outputs && !options.outputs.is_empty() {
        &[]
    } else {
        &SPEED_QUALIFICATION_LADDER
    };
    let (outputs, dropped) = qualification_outputs(
        &options.outputs,
        &previous,
        &requested.outputs(),
        ladder,
        match mode {
            Mode::Command => MAX_QUALIFY_OUTPUTS,
            Mode::Automatic => arcen_session::wayland_qualification::AUTO_REQUALIFY_MAX_OUTPUTS,
        },
    );
    // What this run leaves out is measured by the next one, never lost.
    let mut requested = requested;
    if options.write && !dropped.is_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let requested_path = target.requested_outputs_path();
        match qualification::update_requested_outputs(Path::new(&requested_path), |list| {
            let mut changed = false;
            for output in dropped.iter().rev() {
                changed |= list.note(*output, QUALIFY_CODECS[0], now);
            }
            changed
        }) {
            Ok(kept) => requested = kept,
            Err(error) => step(
                "outputs",
                false,
                &format!("could not keep the displays left out in {requested_path}: {error}"),
            ),
        }
    }
    let list = |outputs: &[QualifyOutput]| {
        outputs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    };
    step(
        "outputs",
        true,
        &format!(
            "{} display(s): {} (operator: [{}]; previous record: [{}]; requested by Decks: [{}]; \
             default ladder: [{}]){}; codecs {}",
            outputs.len(),
            list(&outputs),
            list(&options.outputs),
            list(&previous),
            list(&requested.outputs()),
            list(ladder),
            if dropped.is_empty() {
                String::new()
            } else {
                format!(
                    "; over this run's limit, kept for the next qualification: {}",
                    list(&dropped)
                )
            },
            options.codecs.join(","),
        ),
    );
    let user = QualificationUser::prepare(&options, &target).await?;
    step(
        "user",
        true,
        &format!(
            "{} ({})",
            user.name,
            if user.ephemeral {
                "temporary local account, removed afterwards"
            } else {
                "operator-named local account"
            }
        ),
    );
    // X11 applications are tried unless the operator turned them off or no
    // Xwayland is installed. A session that cannot start with Xwayland is
    // retried without it, so a broken X server never costs the host its
    // Wayland desktop; the record then says why X11 applications are off.
    let try_x11 = config
        .wayland_x11_apps
        .qualifies(super::xwayland_installed());
    let version = if try_x11 {
        Some(xwayland_version().await)
    } else {
        None
    };
    if !try_x11 {
        step(
            "x11_apps",
            true,
            &format!(
                "not tried (platform.session.wayland_x11_apps = {}, Xwayland installed: {})",
                config.wayland_x11_apps.token(),
                super::xwayland_installed()
            ),
        );
    }
    let mut x11_apps: Option<XwaylandProof> = None;
    let speed_minimum = speed_minimum_fps(pipeline_contract(PipelineId::Speed).max_fps);
    let mut results = Vec::new();
    let mut cursor_probe = None;
    let mut native_seat = None;
    let mut measured_outputs = Vec::new();
    let mut multi_output_runs = Vec::new();
    let mut fidelity_outcome = None;
    let interrupted = {
        let started = Instant::now();
        let remote_monitors =
            arcen_session::wayland_outputs::VirtualOutputMode::for_compositor_major(
                arcen_session::wayland_outputs::parse_compositor_major(
                    &identity.stack.compositor_version,
                ),
            )
            .reconfigurable();
        let outputs = async {
            for (index, output) in outputs.iter().enumerate() {
                if index > 0 && past_automatic_deadline(mode, started.elapsed()) {
                    step(
                        "outputs",
                        true,
                        &format!(
                            "time limit reached; {output} and later displays are measured by the \
                             next qualification"
                        ),
                    );
                    break;
                }
                // The display's own refresh first: the lowest refresh that
                // holds Speed is the one a session gets. Only codecs that
                // did not hold it there are measured again at the paced
                // refresh.
                let display_hz = output.display_refresh_hz;
                let with_x11 = match (&version, &x11_apps) {
                    (Some(version), None) if index == 0 => Some(version),
                    (Some(version), Some(proof)) if proof.proven() => Some(version),
                    _ => None,
                };
                let mut result = qualify_output(
                    &config,
                    &user,
                    *output,
                    display_hz,
                    &options.codecs,
                    options.seconds,
                    index == 0,
                    with_x11,
                )
                .await;
                if index == 0 && with_x11.is_some() {
                    if result.started {
                        x11_apps.clone_from(&result.x11);
                    } else {
                        step(
                            "x11_apps",
                            false,
                            "the session did not start with Xwayland; qualifying again without it",
                        );
                        x11_apps = Some(XwaylandProof {
                            display_proven: false,
                            window_manager: false,
                            pointer: false,
                            glamor: GlamorRoute::Unknown,
                            xwayland_version: version.clone().flatten(),
                            failure: Some(
                                "the headless session did not start with Xwayland".to_owned(),
                            ),
                        });
                        result = qualify_output(
                            &config,
                            &user,
                            *output,
                            display_hz,
                            &options.codecs,
                            options.seconds,
                            true,
                            None,
                        )
                        .await;
                    }
                }
                let with_x11 = match (&version, &x11_apps) {
                    (Some(version), Some(proof)) if proof.proven() => Some(version),
                    _ => None,
                };
                let paced = speed_output_refresh_hz(display_hz);
                let short = options
                    .codecs
                    .iter()
                    .filter(|codec| {
                        !result.rates.iter().any(|rate| {
                            &rate.codec == *codec
                                && rate
                                    .sustained_source_fps
                                    .is_some_and(|fps| fps >= speed_minimum)
                        })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if paced != display_hz && !short.is_empty() && result.route.is_some() {
                    let paced_result = qualify_output(
                        &config,
                        &user,
                        *output,
                        paced,
                        &short,
                        options.seconds,
                        false,
                        with_x11,
                    )
                    .await;
                    result.rates.extend(paced_result.rates);
                    result.teardown_clean &= paced_result.teardown_clean;
                }
                results.push(result);
                measured_outputs.push(*output);
            }
            // Grading and HDR, in a session of their own on the stack's wide
            // composition route (when it has one).
            if past_automatic_deadline(mode, started.elapsed()) {
                step(
                    "fidelity_route",
                    true,
                    "time limit reached; Grading and HDR are measured by the next qualification",
                );
            } else if let Some(output) = outputs.first() {
                fidelity_outcome = Some(
                    fidelity::qualify(
                        &config,
                        &user,
                        &identity.stack.compositor_version,
                        (output.width, output.height, output.display_refresh_hz),
                    )
                    .await,
                );
            }
            // Several outputs at once, on a compositor that creates them on
            // demand: the most first, then fewer only if that failed. The
            // operator's first display, else 1920x1080 at 60 Hz. An automatic
            // re-qualification runs it too: the multi-monitor offer rests on
            // it.
            if remote_monitors && options.multi_outputs >= 2 {
                let output = options
                    .outputs
                    .first()
                    .map_or(multi_output::DEFAULT_MULTI_OUTPUT, |output| {
                        (output.width, output.height, output.display_refresh_hz)
                    });
                let gpu_import_verified = results
                    .first()
                    .and_then(|result: &OutputResult| result.route.as_ref())
                    .is_some_and(|proof| {
                        proof.route.gpu_import() && proof.gpu_import_verified_frames > 0
                    });
                let native_encoder = results
                    .first()
                    .map_or(CapabilityFact::Unknown, |result: &OutputResult| {
                        result.native_encoder
                    });
                let counts = if options.multi_outputs > 2 {
                    vec![options.multi_outputs, 2]
                } else {
                    vec![2]
                };
                for count in counts {
                    // An automatic run stays within its bound: what it does
                    // not reach is left for the next qualification.
                    if past_automatic_deadline(mode, started.elapsed()) {
                        step(
                            &format!("multi_output_{count}"),
                            true,
                            "deferred: time limit reached; the multi-monitor offer returns with \
                             the next qualification (arcen-pier wayland qualify --write)",
                        );
                        break;
                    }
                    if let Some(run) = multi_output::qualify_multi_output(
                        &config,
                        &user,
                        output,
                        count,
                        gpu_import_verified,
                        native_encoder,
                    )
                    .await
                    {
                        let proven = run.every_output_encoded();
                        multi_output_runs.push(run);
                        if proven {
                            break;
                        }
                    }
                }
            }
            // Cursor metadata, on a short desktop of its own that records
            // its cursor as metadata.
            if past_automatic_deadline(mode, started.elapsed()) {
                step(
                    "cursor_metadata",
                    true,
                    "deferred: time limit reached; the Deck-drawn cursor returns with the next qualification",
                );
            } else if let Some(first) = outputs.first() {
                cursor_probe = qualify_cursor_metadata(&config, &user, *first).await;
            }
            // A desktop on its own seat with a real pen (one per host).
            if past_automatic_deadline(mode, started.elapsed()) {
                step(
                    "native_seat",
                    true,
                    "deferred: time limit reached; desktops stay headless until the next qualification",
                );
            } else if let Some(first) = outputs.first() {
                native_seat = qualify_native_seat(&config, &user, *first).await;
            }
        };
        let stop_requested = async {
            match stop {
                Some(stop) => {
                    let signal = crate::service_signals::stopped(stop).await;
                    step(
                        "interrupted",
                        false,
                        &format!("{signal}: abandoning the qualification desktop"),
                    );
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        };
        tokio::select! {
            () = outputs => false,
            () = stop_requested => true,
        }
    };
    let released = user.release().await;
    step(
        "user_removed",
        released.is_ok(),
        &match &released {
            Ok(()) if user.ephemeral => format!("{} removed", user.name),
            Ok(()) => format!("{} left as it was", user.name),
            Err(error) => error.clone(),
        },
    );
    if interrupted {
        return Err("interrupted; nothing was recorded".to_owned());
    }

    let route = results.iter().find_map(|result| result.route.clone());
    let input = results.iter().find_map(|result| result.input.clone());
    let facts = QualifiedFacts {
        capture_route: route,
        input,
        source_rates: results
            .iter()
            .flat_map(|result| result.rates.iter().cloned())
            .collect(),
        audio_monitor: match results.first().and_then(|result| result.audio_monitor) {
            Some(true) => CapabilityFact::Available,
            Some(false) => CapabilityFact::Missing,
            None => CapabilityFact::Unknown,
        },
        // Grading and HDR only as the fidelity session proved them.
        grading: fidelity_outcome
            .as_ref()
            .map_or(CapabilityFact::Missing, |outcome| outcome.grading),
        hdr: fidelity_outcome
            .as_ref()
            .map_or(CapabilityFact::Missing, |outcome| outcome.hdr),
        fidelity: fidelity_outcome
            .as_ref()
            .map(|outcome| outcome.proof.clone()),
        multi_output: (!multi_output_runs.is_empty()).then(|| {
            multi_output_runs.sort_by_key(|run| run.outputs.len());
            arcen_session::wayland_qualification::MultiOutputFacts {
                runs: multi_output_runs,
            }
        }),
        x11_apps,
        cursor_metadata: CursorMetadataProbe::fact(cursor_probe.as_ref()),
        native_seat,
        encoder: results.first().and_then(|first| {
            (first.native_encoder != CapabilityFact::Unknown).then(|| EncoderProof {
                native: first.native_encoder,
                capture_route_encoder: results
                    .iter()
                    .find(|result| result.route.is_some())
                    .and_then(|result| result.route_encoder.clone()),
            })
        }),
    };
    let record = WaylandQualificationRecord {
        schema_version: SCHEMA_VERSION,
        qualified_at_unix_s: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
        host: identity.clone(),
        os,
        facts,
    };
    let qualified = record
        .clone()
        .validate(&identity)
        .map_err(|error| error.to_string())?;
    let facts = HostDesktopFacts {
        xorg_server: super::xorg_server_installed(&config.xorg_bin),
        wayland_built: true,
        wayland: qualified.apply(live),
        qualification: None,
    };
    // Whether this host can serve Wayland at all, and what start-up
    // selection will decide with this record and the configured preference.
    let wayland = select_desktop_provider(ProviderPreference::Wayland, &facts);
    let start_up = select_desktop_provider(config.provider_preference, &facts);
    let teardown_clean = results.iter().all(|result| result.teardown_clean);
    println!();
    println!("Supported with this record:");
    let software = qualified.record().proven_native_encoder() == CapabilityFact::Missing;
    println!(
        "  Auto:    {}",
        match (wayland.is_ok(), software) {
            (true, true) =>
                "yes, in software (OpenH264, H.264 4:2:0 at up to 30 fps, outputs \
                             fitted to 1920x1200)",
            (true, false) => "yes",
            (false, _) => "no",
        }
    );
    println!(
        "  Encoder: {}",
        match qualified.record().proven_native_encoder() {
            CapabilityFact::Available => "native (NVENC)",
            CapabilityFact::Missing => "software only: no native encoder opened",
            CapabilityFact::Unknown => "not probed",
        }
    );
    println!(
        "  GPU import: {}",
        match &qualified.record().facts.capture_route {
            Some(proof) if qualified.record().proven_gpu_import() == CapabilityFact::Available => {
                format!(
                    "yes ({}, {} frame(s) verified against a host copy)",
                    proof.route.token(),
                    proof.gpu_import_verified_frames
                )
            }
            Some(proof) => format!("no (route {})", proof.route.token()),
            None => "no".to_owned(),
        }
    );
    let route = qualified.serving_route();
    for output in &outputs {
        let candidates = [
            output.display_refresh_hz,
            speed_output_refresh_hz(output.display_refresh_hz),
        ];
        for codec in &options.codecs {
            let decision = qualified.speed_decision(&SpeedRequest {
                width: output.width,
                height: output.height,
                codec,
                route,
                candidate_refreshes: &candidates,
                ceiling_fps: pipeline_contract(PipelineId::Speed).max_fps,
                minimum_fps: speed_minimum,
            });
            println!(
                "  Speed {output} {codec}: {}",
                decision.map_or_else(
                    || {
                        if software {
                            "no (served as Software at up to 30 fps: no native encoder)"
                                .to_owned()
                        } else {
                            format!(
                                "no (served as Auto: the source did not sustain {speed_minimum} fps)"
                            )
                        }
                    },
                    |decision| format!(
                        "yes, {} at {} fps on a {} Hz output",
                        decision.evidence, decision.measured_fps, decision.output_refresh_hz
                    ),
                )
            );
        }
    }
    println!(
        "  X11 apps: {}",
        match &qualified.record().facts.x11_apps {
            Some(proof) if proof.proven() => format!(
                "yes (glamor {}, Xwayland {})",
                proof.glamor.token(),
                proof.xwayland_version.as_deref().unwrap_or("unknown")
            ),
            Some(proof) => format!("no ({})", proof.failure.as_deref().unwrap_or("not proven")),
            None => "no (not tried)".to_owned(),
        }
    );
    if wayland.is_ok() {
        match arcen_session::wayland_x11::resolve_x11_apps(
            config.wayland_x11_apps,
            super::xwayland_installed(),
            qualified.record().facts.x11_apps.as_ref(),
        ) {
            Ok(plan) => println!(
                "  X11 apps at start-up (platform.session.wayland_x11_apps = {}): {}",
                config.wayland_x11_apps.token(),
                plan.token()
            ),
            Err(refusal) => println!("  X11 apps at start-up: refused: {refusal}"),
        }
    }
    println!(
        "  Multi-monitor: {}",
        match qualified.max_concurrent_outputs() {
            1 => "no (one output proven; `--multi-output N` measures several)".to_owned(),
            outputs => format!("up to {outputs} outputs"),
        }
    );
    match &fidelity_outcome {
        Some(outcome) => {
            println!(
                "  Grading: {}",
                fidelity::summary(outcome, PipelineId::Grading)
            );
            println!("  HDR:     {}", fidelity::summary(outcome, PipelineId::Hdr));
        }
        None => {
            println!("  Grading: no (not qualified)");
            println!("  HDR:     no (not qualified)");
        }
    }
    if let Err(refusal) = &wayland {
        println!("Wayland on this host: refused: {refusal}");
    }
    match &start_up {
        Ok(selection) => println!(
            "Start-up selection with this record (capture.provider = {}): {} ({}).",
            config.provider_preference.token(),
            match selection.provider {
                DesktopProvider::Xorg => "X11",
                DesktopProvider::Wayland => "Wayland",
            },
            selection.reason.token()
        ),
        Err(refusal) => println!(
            "Start-up selection with this record (capture.provider = {}): refused: {refusal}",
            config.provider_preference.token()
        ),
    }
    println!();
    println!(
        "{}",
        serde_json::to_string_pretty(&record).map_err(|error| error.to_string())?
    );
    let passed = wayland.is_ok() && teardown_clean && released.is_ok();
    // The command records a failure too (the Pier then refuses with its
    // reasons); an automatic run keeps the stale record instead, so the
    // next start sees the same version refusal and its attempt marker.
    if mode == Mode::Automatic && !passed {
        println!("Not recorded: the automatic re-qualification did not pass.");
    } else if options.write {
        let path = target.record_path();
        qualification::write_record(Path::new(path), &record)
            .map_err(|error| format!("writing {path}: {error}"))?;
        println!("Recorded in {path}.");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let requested_path = target.requested_outputs_path();
        match qualification::update_requested_outputs(Path::new(&requested_path), |list| {
            let before = list.clone();
            list.entries
                .retain(|entry| !measured_outputs.contains(&entry.output));
            for output in outputs.iter().rev() {
                if !measured_outputs.contains(output) {
                    list.note(*output, QUALIFY_CODECS[0], now);
                }
            }
            *list != before
        }) {
            Ok(remaining) => println!(
                "Requested displays now in {requested_path}: {}.",
                remaining.entries.len()
            ),
            Err(error) => println!("Could not update {requested_path}: {error}"),
        }
    } else {
        println!("Not recorded (pass --write).");
    }
    Ok(passed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pointer_proofs_capture_uses_the_encoder_sessions_get() {
        assert_eq!(
            session_encoder(CapabilityFact::Missing),
            EncoderSelection::SoftwareH264
        );
        assert_eq!(
            session_encoder(CapabilityFact::Available),
            EncoderSelection::NativeNvenc
        );
        assert_eq!(
            session_encoder(CapabilityFact::Unknown),
            EncoderSelection::Auto
        );
    }

    #[test]
    fn an_automatic_run_stops_starting_work_at_its_deadline() {
        let bound =
            Duration::from_secs(arcen_session::wayland_qualification::AUTO_REQUALIFY_DEADLINE_S);
        assert!(!past_automatic_deadline(Mode::Automatic, bound / 2));
        assert!(past_automatic_deadline(Mode::Automatic, bound));
        assert!(!past_automatic_deadline(Mode::Command, bound * 10));
    }

    #[test]
    fn only_an_absent_native_encoder_is_recorded_as_missing() {
        use arcen_media::video::BackendUnavailableReason as Reason;
        for nvidia in [false, true] {
            assert_eq!(
                native_encoder_fact(Some(Reason::HardwareUnavailable), nvidia),
                CapabilityFact::Missing
            );
            assert_eq!(
                native_encoder_fact(Some(Reason::NotBuilt), nvidia),
                CapabilityFact::Missing
            );
            for transient in [
                Reason::SessionLimit,
                Reason::UnsupportedDisplay,
                Reason::UnsupportedConfiguration,
            ] {
                assert_eq!(
                    native_encoder_fact(Some(transient), nvidia),
                    CapabilityFact::Unknown
                );
            }
            assert_eq!(native_encoder_fact(None, nvidia), CapabilityFact::Unknown);
        }
        // No libcuda on an Intel/AMD host: no native encoder exists here.
        assert_eq!(
            native_encoder_fact(Some(Reason::RuntimeMissing), false),
            CapabilityFact::Missing
        );
        // libnvidia-encode missing or a driver mismatch beside a loaded
        // NVIDIA driver: keep trying NVENC.
        assert_eq!(
            native_encoder_fact(Some(Reason::RuntimeMissing), true),
            CapabilityFact::Unknown
        );
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn cursor_metadata_needs_a_sprite_bitmap() {
        let proven = parse_cursor_metadata_probe(
            "[capenc] wayland-probe summary frames=1 elapsed_s=3.00 cursor_metadata=true \
             cursor_bitmap_serial=3 cursor_bitmap=24x24/fmt11 last_refusal=None",
        )
        .unwrap();
        assert_eq!(proven.bitmap.as_deref(), Some("24x24/fmt11"));
        assert_eq!(
            CursorMetadataProbe::fact(Some(&proven)),
            CapabilityFact::Available
        );
        let positions_only = parse_cursor_metadata_probe(
            "wayland-probe summary frames=1 cursor_metadata=true cursor_bitmap=-",
        )
        .unwrap();
        assert_eq!(
            CursorMetadataProbe::fact(Some(&positions_only)),
            CapabilityFact::Missing
        );
        let none = parse_cursor_metadata_probe(
            "wayland-probe summary frames=1 cursor_metadata=false cursor_bitmap=-",
        )
        .unwrap();
        assert_eq!(
            CursorMetadataProbe::fact(Some(&none)),
            CapabilityFact::Missing
        );
        assert_eq!(CursorMetadataProbe::fact(None), CapabilityFact::Unknown);
        assert!(parse_cursor_metadata_probe("wayland-probe frame 1").is_none());
    }

    #[test]
    fn options_default_to_the_temporary_user_and_no_operator_outputs() {
        let options = QualifyOptions::parse(&args(&["--write", "--no-config"])).unwrap();
        assert!(options.write);
        assert_eq!(options.user, None);
        // No operator size: the previous record's, the requested and the
        // ladder are measured (`qualification_outputs`).
        assert!(options.outputs.is_empty());
        assert!(!options.replace_outputs);
        assert_eq!(options.codecs, vec!["h265".to_owned(), "h264".to_owned()]);
        assert_eq!(options.seconds, DEFAULT_MEASURE_SECONDS);
        assert_eq!(options.config, None);
    }

    #[test]
    fn options_name_a_local_user_outputs_and_a_rate_window() {
        let options = QualifyOptions::parse(&args(&[
            "--user",
            "labtest",
            "--password-file",
            "/root/pw",
            "--output",
            "2560x1440@60",
            "--output",
            "1920x1080",
            "--output",
            "1920x1080@60",
            "--seconds",
            "12",
            "--codec",
            "h264",
            "--replace-outputs",
        ]))
        .unwrap();
        assert_eq!(
            options.user,
            Some(("labtest".to_owned(), PathBuf::from("/root/pw")))
        );
        assert_eq!(
            options.outputs,
            vec![
                QualifyOutput::new(2560, 1440, 60).unwrap(),
                QualifyOutput::new(1920, 1080, 60).unwrap()
            ]
        );
        assert_eq!(options.codecs, vec!["h264".to_owned()]);
        assert!(options.replace_outputs);
        assert_eq!(options.seconds, 12);
    }

    #[test]
    fn options_refuse_what_they_cannot_honour() {
        for bad in [
            &["--user", "labtest"][..],
            &["--password-file", "/root/pw"],
            &["--output", "huge"],
            &["--output", "99999x10@60"],
            &["--seconds", "2"],
            &["--codec", "mjpeg"],
            &["--config", "/a", "--no-config"],
            &["--surprise"],
            &["--record", "/a.json", "--record", "/b.json"],
            &[
                "--temporary-user",
                "arcena",
                "--user",
                "u",
                "--password-file",
                "/p",
            ],
        ] {
            assert!(QualifyOptions::parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_record_comes_from_the_flag_then_the_configuration() {
        use arcen_session::wayland_qualification::RECORD_PATH;
        let production = QualificationTarget::production();
        let configured =
            QualificationTarget::for_record("/var/lib/arcen-dev/cfg/q.json", None).unwrap();
        let options = QualifyOptions::parse(&args(&["--no-config"])).unwrap();
        assert!(!options.allow_running_service);
        assert_eq!(options.target(&production).unwrap(), production);
        assert_eq!(options.target(&configured).unwrap(), configured);
        // --record wins over the configuration.
        let flagged = QualifyOptions::parse(&args(&[
            "--no-config",
            "--record",
            "/var/lib/arcen-dev/speed/q.json",
            "--allow-running-service",
        ]))
        .unwrap();
        assert!(flagged.allow_running_service);
        let target = flagged.target(&configured).unwrap();
        assert_eq!(target.record_path(), "/var/lib/arcen-dev/speed/q.json");
        assert_ne!(target.temporary_user(), production.temporary_user());
        // A named temporary account applies to the configured record, but
        // never to the production one.
        let named =
            QualifyOptions::parse(&args(&["--no-config", "--temporary-user", "arcenspd"])).unwrap();
        assert_eq!(
            named.target(&configured).unwrap().temporary_user(),
            "arcenspd"
        );
        assert!(named.target(&production).is_err());
        let production_flag =
            QualifyOptions::parse(&args(&["--no-config", "--record", RECORD_PATH])).unwrap();
        assert_eq!(production_flag.target(&configured).unwrap(), production);
        assert!(
            QualifyOptions::parse(&args(&["--no-config", "--record", "relative.json"]))
                .unwrap()
                .target(&production)
                .is_err()
        );
    }

    #[test]
    fn the_gpu_import_check_is_read_from_the_harness_line() {
        let ok = parse_gpu_import_verify(
            "[capenc] wayland-probe gpu-import-verify result=ok pixels=2073600 mismatched=0 \
             max_channel_diff=0 discriminating=true gpu_frames=1 sync=explicit",
        )
        .unwrap();
        assert!(ok.verified());
        assert_eq!(ok.gpu_frames, 1);
        assert_eq!(ok.sync, "explicit");
        let mismatch = parse_gpu_import_verify(
            "wayland-probe gpu-import-verify result=MISMATCH pixels=10 mismatched=10 \
             max_channel_diff=255 discriminating=true gpu_frames=1 sync=implicit",
        )
        .unwrap();
        assert!(!mismatch.verified());
        let none = parse_gpu_import_verify(
            "[capenc] wayland-probe gpu-import-verify result=UNVERIFIED (no DMA-BUF frame)",
        )
        .unwrap();
        assert!(!none.verified());
        assert_eq!(
            parse_gpu_import_verify("[capenc] wayland-probe summary frames=2"),
            None
        );
        // A Speed session for a 60 Hz display runs its output at 120 Hz; a
        // faster display keeps its own rate.
        assert_eq!(speed_output_refresh_hz(60), 120);
        assert_eq!(speed_output_refresh_hz(144), 144);
    }

    #[test]
    fn the_temporary_password_is_long_and_unambiguous() {
        let password = random_password().unwrap();
        assert_eq!(password.len(), 32);
        assert!(password
            .chars()
            .all(|character| character.is_ascii_alphanumeric()));
    }
}
