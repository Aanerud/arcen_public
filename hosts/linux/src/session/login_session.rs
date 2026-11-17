// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Arcen's real login session on a host without a display manager (ADR
//! 0025 §10): the Linux adapter.
//!
//! Every decision is `arcen_session::console_session::login_session`'s.
//! The console launcher asks systemd to start `arcen-pier login-session`
//! for an account the Pier has just authenticated ([`start`]); that
//! process does what a display manager does ([`login_session_main`]): it
//! opens a PAM session with Arcen's own service (no authentication; account
//! and session lines are the host's `login` ones), which places it in the
//! user's logind session on `seat0` at a virtual terminal, starts an X
//! server there and runs the account's X session entry through the
//! system's `Xsession`, then waits until the user logs out. It belongs to
//! systemd and logind, not to a Deck's connection or to the Pier: the
//! console launcher joins it like any console session, a Deck's departure
//! locks it, and a Pier restart leaves it running.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use arcen_session::console_session::{self as model, login_session as policy, ConsoleRefusal};
use tokio::process::Command;

use super::identity::{configure_user_command, SessionEnvironment, UserIdentity};
use super::launcher::{LauncherError, XorgSeat};
use crate::logging::target;

/// systemd's name for the configured display manager.
const DISPLAY_MANAGER_UNIT: &str = "/etc/systemd/system/display-manager.service";
/// How long a new session may take to appear in logind, active.
/// Inside the broker's console open (55 s) with the display wait (15 s)
/// and the steps before it.
const START_TIMEOUT: Duration = Duration::from_secs(25);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the host has a display manager (`systemctl enable gdm` and its
/// kind link `display-manager.service`).
pub(crate) fn display_manager_configured() -> bool {
    std::fs::symlink_metadata(DISPLAY_MANAGER_UNIT).is_ok()
}

/// The host facts a console decision needs beside logind's sessions.
pub(crate) fn console_host() -> model::ConsoleHost {
    model::ConsoleHost {
        display_manager: display_manager_configured(),
    }
}

/// What the console launcher passes on to a new login session.
#[derive(Debug, Clone)]
pub(crate) struct LoginLaunch {
    pub(crate) xorg_binary: PathBuf,
    pub(crate) xorg_config_template: PathBuf,
    pub(crate) runtime_root: PathBuf,
    pub(crate) gpu_head: String,
    /// The Pier's own desktop display (`platform.session.display`), never
    /// taken by a login session.
    pub(crate) reserved_display: String,
    pub(crate) desktop: String,
    /// The depth the session's X server is created at
    /// (`video.x11_desktop_depth`, decided for the first sign-in).
    pub(crate) depth: Option<u8>,
}

fn stack_installed() -> bool {
    std::fs::read_to_string(format!("/etc/pam.d/{}", policy::PAM_SERVICE))
        .is_ok_and(|text| policy::pam_stack_is_arcen(&text))
}

fn taken_displays() -> Vec<u32> {
    // Abstract sockets too: an X server needs no file in /tmp to hold a
    // display, and clients try the abstract name first.
    let mut taken: Vec<u32> = std::fs::read_to_string("/proc/net/unix")
        .map(|table| {
            table
                .lines()
                .filter_map(|line| line.split_whitespace().nth(7))
                .filter_map(|path| path.strip_prefix("@/tmp/.X11-unix/X"))
                .filter_map(|n| n.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir("/tmp/.X11-unix") {
        for entry in entries.flatten() {
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix('X'))
                .and_then(|n| n.parse().ok())
            {
                taken.push(n);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(".X"))
                .and_then(|name| name.strip_suffix("-lock"))
                .and_then(|n| n.parse().ok())
            {
                taken.push(n);
            }
        }
    }
    taken
}

async fn bounded(mut command: Command) -> Option<std::process::Output> {
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    match tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Some(output),
        _ => None,
    }
}

/// The systemd unit a login session of `uid` runs in while it starts.
fn unit_name(uid: u32) -> String {
    format!("arcen-login-{uid}.service")
}

/// Starts the account's login session at virtual terminal `vt` and returns
/// its logind session once it is active on `seat0`.
pub(crate) async fn start(
    identity: &UserIdentity,
    vt: u32,
    launch: &LoginLaunch,
) -> Result<String, ConsoleRefusal> {
    if !stack_installed() {
        tracing::error!(
            target: target::SESSION,
            pam_service = policy::PAM_SERVICE,
            "Arcen's login service is not installed (rerun the installer); a sign-in on this host without a display manager is refused"
        );
        return Err(ConsoleRefusal::SignInUnavailable);
    }
    if let Some(refusal) =
        super::console::domain_refusal(&identity.username, policy::PAM_SERVICE).await
    {
        return Err(refusal);
    }
    let reserved: Vec<u32> = model::local_display_number(&launch.reserved_display)
        .into_iter()
        .collect();
    let Some(display_number) = policy::choose_display(&taken_displays(), &reserved) else {
        tracing::error!(target: target::SESSION, "no X display number is free for a login session");
        return Err(ConsoleRefusal::SignInFailed);
    };
    let Ok(exe) = std::env::current_exe() else {
        return Err(ConsoleRefusal::SignInFailed);
    };
    let unit = unit_name(identity.uid);
    let mut run = Command::new("/usr/bin/systemd-run");
    run.args([
        "--quiet",
        "--collect",
        &format!("--unit={unit}"),
        "--service-type=exec",
        "--property=KillMode=process",
        &format!(
            "--description=Arcen login session of {} (VT {vt})",
            identity.username
        ),
        "--",
    ])
    .arg(&exe)
    .args(["login-session", "--user", &identity.username])
    .args(["--uid", &identity.uid.to_string()])
    .args(["--vt", &vt.to_string()])
    .args(["--display", &format!(":{display_number}")])
    .args(["--desktop", &launch.desktop])
    .arg("--runtime-root")
    .arg(&launch.runtime_root)
    .arg("--xorg-bin")
    .arg(&launch.xorg_binary)
    .arg("--xorg-config-template")
    .arg(&launch.xorg_config_template)
    .args(["--gpu-head", &launch.gpu_head]);
    if let Some(depth) = launch.depth {
        run.args(["--depth", &depth.to_string()]);
    }
    match bounded(run).await {
        Some(output) if output.status.success() => {}
        output => {
            tracing::error!(
                target: target::SESSION,
                unit,
                stderr = %output.map(|o| String::from_utf8_lossy(&o.stderr).trim().to_owned()).unwrap_or_default(),
                "the login session could not be started"
            );
            return Err(ConsoleRefusal::SignInFailed);
        }
    }
    tracing::info!(
        target: target::SESSION,
        username = %identity.username,
        unit,
        vt,
        x_display = %format!(":{display_number}"),
        "starting the account's login session (no display manager on this host)"
    );
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let mut polls: u32 = 0;
    while tokio::time::Instant::now() < deadline {
        polls = polls.wrapping_add(1);
        // A session that failed to start ended its unit: say so now.
        if polls.is_multiple_of(4) && !unit_active(&unit).await {
            tracing::error!(
                target: target::SESSION,
                unit,
                "the login session ended before it became active (journalctl -u {unit})"
            );
            return Err(ConsoleRefusal::SignInFailed);
        }
        if let Some(session) = super::console::read_sessions().await.into_iter().find(|s| {
            s.uid == identity.uid
                && s.is_console_desktop()
                && s.origin() == model::ConsoleOrigin::Arcen
                && s.active
        }) {
            tracing::info!(
                target: target::SESSION,
                session_id = %session.id,
                vt = session.vtnr,
                "the login session is active on seat0"
            );
            return Ok(session.id);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tracing::error!(
        target: target::SESSION,
        unit,
        "the login session did not become active in time; it is stopped (journalctl -u {unit})"
    );
    let mut stop = Command::new("/usr/bin/systemctl");
    stop.args(["stop", &unit]);
    let _ = bounded(stop).await;
    Err(ConsoleRefusal::SignInFailed)
}

async fn unit_active(unit: &str) -> bool {
    let mut show = Command::new("/usr/bin/systemctl");
    show.args(["is-active", "--quiet", unit]);
    bounded(show)
        .await
        .is_some_and(|output| output.status.success())
}

fn argument(args: &[String], flag: &str) -> Result<String, LauncherError> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .ok_or(LauncherError::Protocol)
}

/// `arcen-pier login-session …`, run by systemd for the console launcher.
/// Its log goes to stderr, which systemd keeps in the journal of its unit.
pub fn login_session_main(args: &[String]) -> ExitCode {
    let observability = match arcen_observability::ObservabilityBuilder::new(
        arcen_telemetry::TelemetryRole::Host,
        arcen_telemetry::TelemetryComponent::new("login_session")
            .expect("login_session is a valid canonical component"),
        arcen_telemetry::TelemetryPlatform::Linux,
        arcen_telemetry::OperationalProfile::Critical,
    )
    .canonical_writer("stderr", std::io::stderr())
    .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("login-session: logging setup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    if tracing::dispatcher::set_global_default(observability.dispatch()).is_err() {
        return ExitCode::FAILURE;
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    let exit_code = match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(target: target::SESSION, %error, "the login session ended with an error");
            ExitCode::FAILURE
        }
    };
    runtime.shutdown_timeout(Duration::from_millis(500));
    let _ = observability.guard().shutdown(Duration::from_secs(2));
    exit_code
}

struct NoConversation;

impl pam_client2::ConversationHandler for NoConversation {
    fn prompt_echo_on(
        &mut self,
        _prompt: &std::ffi::CStr,
    ) -> Result<std::ffi::CString, pam_client2::ErrorCode> {
        Err(pam_client2::ErrorCode::CONV_ERR)
    }
    fn prompt_echo_off(
        &mut self,
        _prompt: &std::ffi::CStr,
    ) -> Result<std::ffi::CString, pam_client2::ErrorCode> {
        Err(pam_client2::ErrorCode::CONV_ERR)
    }
    fn text_info(&mut self, _message: &std::ffi::CStr) {}
    fn error_msg(&mut self, _message: &std::ffi::CStr) {}
}

/// The session entry's command for `desktop`.
fn session_command(desktop: &str) -> Result<Vec<String>, LauncherError> {
    for name in policy::xsession_names(desktop) {
        let path = Path::new(policy::XSESSIONS_DIR).join(name);
        if let Ok(entry) = std::fs::read_to_string(&path) {
            if let Some(words) = policy::xsession_exec(&entry) {
                tracing::info!(target: target::SESSION, entry = %path.display(), command = %words.join(" "), "the account's X session");
                return Ok(words);
            }
        }
    }
    tracing::error!(target: target::SESSION, desktop, "no usable X session entry in /usr/share/xsessions");
    Err(LauncherError::Protocol)
}

async fn run(args: &[String]) -> Result<(), LauncherError> {
    use pam_client2::{Context, Flag};
    if !nix::unistd::Uid::effective().is_root() {
        return Err(LauncherError::RootRequired);
    }
    let username = argument(args, "--user")?;
    let uid: u32 = argument(args, "--uid")?
        .parse()
        .map_err(|_| LauncherError::Protocol)?;
    let vt: u32 = argument(args, "--vt")?
        .parse()
        .ok()
        .filter(|vt| (2..=63).contains(vt))
        .ok_or(LauncherError::Protocol)?;
    let x_display = argument(args, "--display")?;
    model::local_display_number(&x_display).ok_or(LauncherError::Protocol)?;
    let desktop = argument(args, "--desktop")?;
    let runtime_root = PathBuf::from(argument(args, "--runtime-root")?);
    let xorg_binary = PathBuf::from(argument(args, "--xorg-bin")?);
    let template = PathBuf::from(argument(args, "--xorg-config-template")?);
    let gpu_head = argument(args, "--gpu-head")?;
    let depth = match argument(args, "--depth") {
        Ok(depth) => Some(
            depth
                .parse::<u8>()
                .ok()
                .and_then(arcen_media::video::DesktopDepth::from_x11_depth)
                .ok_or(LauncherError::Protocol)?,
        ),
        Err(_) => None,
    };
    let identity = UserIdentity::resolve(&username)?;
    if identity.uid != uid || identity.uid == 0 {
        return Err(LauncherError::Protocol);
    }
    let command = session_command(&desktop)?;

    let mut context = Context::new(policy::PAM_SERVICE, Some(&username), NoConversation)
        .map_err(|_| LauncherError::PamInitialization)?;
    let current = if desktop == "gnome-classic" {
        "GNOME-Classic:GNOME"
    } else {
        "GNOME"
    };
    context
        .set_tty(Some(&format!("/dev/tty{vt}")))
        .and_then(|()| context.set_xdisplay(Some(&x_display)))
        .and_then(|()| context.putenv("XDG_SESSION_TYPE=x11"))
        .and_then(|()| context.putenv("XDG_SESSION_CLASS=user"))
        .and_then(|()| context.putenv(format!("XDG_SEAT={}", model::CONSOLE_SEAT)))
        .and_then(|()| context.putenv(format!("XDG_VTNR={vt}")))
        .and_then(|()| context.putenv(format!("XDG_SESSION_DESKTOP={desktop}")))
        .and_then(|()| context.putenv(format!("XDG_CURRENT_DESKTOP={current}")))
        .and_then(|()| context.putenv(format!("DESKTOP_SESSION={desktop}")))
        .map_err(|_| LauncherError::PamInitialization)?;
    // The Pier authenticated the account with its own service; this one
    // checks the account again and opens the session, as a display
    // manager does after its own authentication.
    context
        .acct_mgmt(Flag::NONE)
        .map_err(|_| LauncherError::Rejected)?;
    let pam_session = context
        .open_session(Flag::NONE)
        .map_err(|_| LauncherError::PamSession)?;
    let exec_context = super::launcher::take_session_exec_context()?;
    let pam_environment: Vec<(String, String)> = pam_session
        .envlist()
        .iter_tuples()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let session_id = pam_environment
        .iter()
        .find_map(|(key, value)| (key == "XDG_SESSION_ID").then(|| value.clone()))
        .ok_or(LauncherError::LogindSession)?;
    tracing::info!(target: target::SESSION, session_id, username, vt, x_display = %x_display, "login session opened");

    let provider = super::launcher::DedicatedXorgProvider::new(
        &xorg_binary,
        &template,
        &runtime_root,
        &x_display,
        &gpu_head,
        &session_id,
        &identity,
        None,
        arcen_outputs::SessionScreen {
            depth,
            served_size: None,
        },
        XorgSeat::Console { vt },
    );
    let mut transaction = arcen_outputs::OutputTransaction::acquire(
        provider,
        &None,
        &arcen_outputs::OutputContext::new(
            arcen_telemetry::CorrelationId::new(format!("login-session-{session_id}"))
                .map_err(|_| LauncherError::Protocol)?,
        ),
    )
    .await
    .map_err(|_| LauncherError::XorgStart)?;
    transaction
        .commit()
        .await
        .map_err(|_| LauncherError::XorgStart)?;
    let (_provider, binding) = transaction
        .into_committed()
        .map_err(|_| LauncherError::XorgStart)?
        .into_parts();
    let mut xorg = binding.into_output();
    let environment = SessionEnvironment::build(
        &identity,
        &x_display,
        Some(xorg.xauthority()),
        &desktop,
        None,
        pam_environment,
    )
    .with_locale(&super::locale::session_locale(&identity.username))
    .with_exec_context(exec_context);
    if let Err(error) = super::launcher::activate_logind_session(&environment).await {
        tracing::warn!(target: target::SESSION, %error, "the login session is not active on seat0 yet");
    }
    let wrapper = policy::XSESSION_WRAPPERS
        .iter()
        .map(Path::new)
        .find(|path| path.is_file());
    let mut session = if let Some(wrapper) = wrapper {
        // `Xsession` honours a requested session only as one argument (as
        // a display manager passes it); the words hold no shell syntax.
        let mut session = Command::new(wrapper);
        session.arg(command.join(" "));
        session
    } else {
        let mut session = Command::new(&command[0]);
        session.args(&command[1..]);
        session
    };
    session
        .current_dir(&identity.home)
        .stdin(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(false);
    configure_user_command(&mut session, &identity, &environment)?;
    let mut child = session.spawn()?;
    tracing::info!(target: target::SESSION, session_id, pid = child.id(), "the account's desktop session started");

    let mut stop = super::launcher::StopSignals::install()?;
    let group = child.id();
    let end_session = |signal| {
        if let Some(pid) = group {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid.cast_signed()), signal);
        }
    };
    tokio::select! {
        status = child.wait() => {
            tracing::info!(target: target::SESSION, session_id, ?status, "the user logged out; the login session ends");
        }
        status = xorg.wait_for_exit() => {
            tracing::warn!(target: target::SESSION, session_id, ?status, "the session's X server exited; the login session ends");
            end_session(nix::sys::signal::Signal::SIGTERM);
        }
        () = stop.recv() => {
            tracing::info!(target: target::SESSION, session_id, "stopped; the login session ends");
            end_session(nix::sys::signal::Signal::SIGTERM);
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        }
    }
    xorg.shutdown().await;
    drop(pam_session);
    Ok(())
}
