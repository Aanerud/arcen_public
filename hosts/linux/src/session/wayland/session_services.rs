//! The GNOME login services of a headless session, assembled by the agent.
//!
//! See [`arcen_session::desktop_session`] for why an Arcen desktop does
//! this itself rather than running `gnome-session`. This module is the
//! Linux glue the stack calls at fixed points of its start:
//!
//! 1. before the session bus: the per-session settings overlay (dconf
//!    locks that pin "never suspend", and the lock policy), GNOME's
//!    first-login marker when the operator chose `skip`, and the
//!    autostart plan;
//! 2. right after the bus: the welcome-tour marker (`skip`), the session
//!    manager and the unlocked keyring;
//! 3. once the compositor runs: the settings daemons and autostart
//!    programs (application-phase ones once the desktop is ready).
//!
//! Nothing here is a readiness step and nothing here is fatal: each part
//! that fails is logged, and the desktop runs as it did without it.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use arcen_session::desktop_session::first_login::{
    shell_version_from_banner, welcome_marker_needs_update, FirstLoginSetup,
    INITIAL_SETUP_DONE_CONTENT, INITIAL_SETUP_DONE_MARKER, WELCOME_DIALOG_KEY,
    WELCOME_DIALOG_SCHEMA,
};
use arcen_session::desktop_session::lock::{LockFacts, LockPolicy};
use arcen_session::desktop_session::settings_overlay::{
    compose_profile, pinned_keys, render_keyfile, render_locks, MAX_PROFILE_BYTES,
};
use arcen_session::wayland_session::HeadlessCompositor;
use tokio::sync::watch;
use zeroize::Zeroizing;

use super::autostart::{AutostartPlan, AutostartSupervisor};
use super::session_manager::SessionManager;
use super::stack::MemberEnvironment;
use crate::logging::target;

const DCONF: &str = "/usr/bin/dconf";
const GSETTINGS: &str = "/usr/bin/gsettings";
const OVERLAY_DIR: &str = "dconf";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const GDM_TIMEOUT: Duration = Duration::from_secs(2);
/// GNOME Shell's own `XDG_CURRENT_DESKTOP` for a `gnome` session.
const CURRENT_DESKTOP: &str = "GNOME";

/// What the agent hands the stack for the session's services.
#[derive(Default)]
pub struct SessionServicesConfig {
    /// The login password the launcher handed over, for the keyring.
    pub keyring_password: Option<Zeroizing<Vec<u8>>>,
    /// The operator's first-login policy.
    pub first_login: FirstLoginSetup,
    /// Variables the user's login shell adds.
    pub login_environment: Vec<(String, String)>,
    /// The logind session id (for autostart startup ids).
    pub session_id: String,
}

impl std::fmt::Debug for SessionServicesConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionServicesConfig")
            .field(
                "keyring_password",
                &self.keyring_password.as_ref().map(|_| "<redacted>"),
            )
            .field("first_login", &self.first_login)
            .field("login_environment", &self.login_environment.len())
            .field("session_id", &self.session_id)
            .finish()
    }
}

/// The running services of one session.
pub struct SessionServices {
    session_id: String,
    first_login: FirstLoginSetup,
    plan: Option<AutostartPlan>,
    manager: Option<SessionManager>,
    supervisor: Option<AutostartSupervisor>,
    ready: watch::Sender<bool>,
}

impl std::fmt::Debug for SessionServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionServices")
            .field("manager", &self.manager.is_some())
            .field("supervisor", &self.supervisor.is_some())
            .finish_non_exhaustive()
    }
}

impl SessionServices {
    /// Services that do nothing (a stack started without them).
    #[must_use]
    pub fn none() -> Self {
        Self {
            session_id: String::new(),
            first_login: FirstLoginSetup::Run,
            plan: None,
            manager: None,
            supervisor: None,
            ready: watch::channel(false).0,
        }
    }

    /// Step 1, before the session bus starts: settings overlay, lock
    /// policy, the first-login marker, the autostart plan. Adds what the
    /// members need to `members`.
    pub async fn before_bus(
        config: &SessionServicesConfig,
        members: &mut MemberEnvironment,
    ) -> Self {
        for (name, value) in &config.login_environment {
            members.set(name, value);
        }
        members.set("XDG_MENU_PREFIX", "gnome-");
        members.set("GNOME_DESKTOP_SESSION_ID", "this-is-deprecated");
        let lock = LockPolicy::resolve(&LockFacts {
            gdm_version: gdm_version().await,
        });
        tracing::info!(
            target: target::SESSION,
            lock = lock.token(),
            "session lock policy (GNOME Shell locks only when GDM can unlock it)"
        );
        match write_overlay(members, lock).await {
            Ok(profile) => {
                members.set("DCONF_PROFILE", &profile.to_string_lossy());
                tracing::info!(target: target::SESSION, profile = %profile.display(), "session settings overlay active: suspend and power-key actions pinned to nothing");
            }
            Err(reason) => {
                tracing::warn!(target: target::SESSION, reason, "session settings overlay not active; the session manager still keeps the power daemon from acting")
            }
        }
        if config.first_login == FirstLoginSetup::Skip {
            mark_initial_setup_done(members);
        }
        // The session's own view (the login shell may relocate them), as
        // the programs started below will see them.
        let config_home = super::autostart::config_home(members);
        let directories = super::autostart::autostart_directories(
            &config_home,
            members.var("XDG_CONFIG_DIRS").as_deref(),
        );
        let merged = super::autostart::read_autostart(&directories);
        let plan = super::autostart::plan(merged, members, CURRENT_DESKTOP).await;
        Self {
            session_id: config.session_id.clone(),
            first_login: config.first_login,
            plan: Some(plan),
            manager: None,
            supervisor: None,
            ready: watch::channel(false).0,
        }
    }

    /// Step 2, once the session bus answers: the welcome-tour marker, the
    /// session manager and the keyring. Returns the keyring daemon (an
    /// optional member for the stack) and adds its variables to `members`.
    pub async fn after_bus(
        &mut self,
        password: Option<Zeroizing<Vec<u8>>>,
        compositor: HeadlessCompositor,
        members: &mut MemberEnvironment,
    ) -> Option<tokio::process::Child> {
        if self.first_login == FirstLoginSetup::Skip && compositor == HeadlessCompositor::GnomeShell
        {
            raise_welcome_marker(members, compositor).await;
        }
        match SessionManager::start(members.bus_address(), members.clone()).await {
            Ok(manager) => self.manager = Some(manager),
            Err(error) => {
                tracing::warn!(target: target::SESSION, %error, "session manager not available; the desktop runs without presence, idle lock or logout")
            }
        }
        let components = self
            .plan
            .as_ref()
            .map(|plan| plan.keyring_components.clone())
            .unwrap_or_default();
        let unlock = password.is_some();
        match super::keyring::start(members, &components, password).await {
            Ok(started) => {
                for (name, value) in &started.environment {
                    members.set(name, value);
                }
                tracing::info!(
                    target: target::SESSION,
                    components = %components.join(","),
                    unlock,
                    "keyring daemon started on the private bus"
                );
                tokio::spawn(super::keyring::report_login_keyring(
                    members.bus_address().to_owned(),
                    unlock,
                ));
                Some(started.child)
            }
            Err(reason) => {
                tracing::info!(target: target::SESSION, reason, "keyring not started by the session; applications activate it locked");
                None
            }
        }
    }

    /// Step 3, once the compositor's socket exists: the settings daemons
    /// and autostart programs.
    pub fn after_compositor(&mut self, members: &MemberEnvironment) {
        let Some(plan) = self.plan.take() else { return };
        let environment = self.manager.as_ref().map_or_else(
            || watch::channel(BTreeMap::new()).1,
            SessionManager::environment,
        );
        self.supervisor = Some(AutostartSupervisor::start(
            plan,
            members.clone(),
            self.ready.subscribe(),
            environment,
            &self.session_id,
        ));
    }

    /// The desktop is ready: application-phase autostart programs start.
    pub fn desktop_ready(&self) {
        let _ = self.ready.send(true);
    }

    /// Resolves once the user confirmed a logout inside the session; never
    /// without a session manager.
    pub fn logout_receiver(&self) -> Option<LogoutReceiver> {
        self.manager
            .as_ref()
            .map(|manager| LogoutReceiver(manager.logout()))
    }

    /// Orderly stop, before the compositor: clients are told to stop, then
    /// every supervised program is stopped.
    pub async fn stop(&mut self) {
        if let Some(manager) = self.manager.as_ref() {
            manager.stop_clients().await;
        }
        if let Some(supervisor) = self.supervisor.as_mut() {
            supervisor.stop().await;
        }
        self.supervisor = None;
    }

    /// Drops the session manager's bus connection (before the bus stops).
    pub fn release_bus(&mut self) {
        self.manager = None;
    }
}

/// A logout confirmation the agent's serve loop waits on.
#[derive(Debug)]
pub struct LogoutReceiver(watch::Receiver<bool>);

impl LogoutReceiver {
    /// Resolves once a logout was confirmed.
    pub async fn confirmed(&mut self) {
        while !*self.0.borrow_and_update() {
            if self.0.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// GDM's version on the system bus, as GNOME Shell's `canLock` asks it.
async fn gdm_version() -> Option<String> {
    tokio::time::timeout(GDM_TIMEOUT, async {
        let connection = zbus::Connection::system().await.ok()?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.gnome.DisplayManager",
            "/org/gnome/DisplayManager/Manager",
            "org.gnome.DisplayManager.Manager",
        )
        .await
        .ok()?;
        proxy.get_property::<String>("Version").await.ok()
    })
    .await
    .ok()
    .flatten()
}

/// Writes and compiles the session's dconf database and profile; returns
/// the profile path.
async fn write_overlay(
    members: &MemberEnvironment,
    lock: LockPolicy,
) -> Result<PathBuf, &'static str> {
    if !Path::new(DCONF).is_file() {
        return Err("dconf is not installed");
    }
    let directory = members.runtime_dir().join(OVERLAY_DIR);
    let keyfiles = directory.join("arcen.d");
    let locks = keyfiles.join("locks");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&locks)
        .map_err(|_| "the overlay directory could not be created")?;
    let keys = pinned_keys(lock);
    std::fs::write(keyfiles.join("00-arcen"), render_keyfile(&keys))
        .and_then(|()| std::fs::write(locks.join("00-arcen"), render_locks(&keys)))
        .map_err(|_| "the overlay keyfile could not be written")?;
    let database = directory.join("arcen.db");
    let mut compile = members.command(DCONF);
    compile.arg("compile").arg(&database).arg(&keyfiles);
    let compiled = tokio::time::timeout(COMMAND_TIMEOUT, compile.status())
        .await
        .ok()
        .and_then(Result::ok)
        .is_some_and(|status| status.success());
    if !compiled || !database.is_file() {
        return Err("dconf compile failed");
    }
    let base = base_profile();
    let profile = compose_profile(&database.to_string_lossy(), base.as_deref())
        .ok_or("the overlay path is not usable in a profile")?;
    let path = directory.join("profile");
    std::fs::write(&path, profile).map_err(|_| "the profile could not be written")?;
    Ok(path)
}

/// The dconf profile this user would otherwise get: `$DCONF_PROFILE`, then
/// `/etc/dconf/profile/user`, then `$XDG_DATA_DIRS/dconf/profile/user`.
fn base_profile() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(profile) = std::env::var("DCONF_PROFILE") {
        if profile.starts_with('/') {
            candidates.push(PathBuf::from(profile));
        } else if !profile.contains('/') && !profile.is_empty() {
            candidates.push(Path::new("/etc/dconf/profile").join(profile));
        }
    }
    candidates.push(PathBuf::from("/etc/dconf/profile/user"));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    for dir in data_dirs.split(':').filter(|dir| dir.starts_with('/')) {
        candidates.push(Path::new(dir).join("dconf/profile/user"));
    }
    candidates.into_iter().find_map(|path| {
        let metadata = std::fs::metadata(&path).ok()?;
        (metadata.is_file() && metadata.len() <= MAX_PROFILE_BYTES as u64)
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten()
    })
}

/// `wayland.first_login_setup = skip`: writes GNOME's own initial-setup
/// marker for the user (never overwriting one), as finishing the assistant
/// would have.
fn mark_initial_setup_done(members: &MemberEnvironment) {
    let config_home = super::autostart::config_home(members);
    let marker = config_home.join(INITIAL_SETUP_DONE_MARKER);
    let created = std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&config_home)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(&marker)
        })
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(INITIAL_SETUP_DONE_CONTENT.as_bytes())
        });
    match created {
        Ok(()) => {
            tracing::info!(target: target::SESSION, "first login: skipped by policy; GNOME's initial-setup marker written")
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            tracing::warn!(target: target::SESSION, %error, "first login: the initial-setup marker could not be written")
        }
    }
}

/// `skip`: raises the shell's welcome-tour marker to the running shell's
/// version, as the shell itself does after showing the tour.
async fn raise_welcome_marker(members: &MemberEnvironment, compositor: HeadlessCompositor) {
    let run = |args: Vec<String>, binary: &str| {
        let mut command = members.command(binary);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        async move {
            tokio::time::timeout(COMMAND_TIMEOUT, command.output())
                .await
                .ok()
                .and_then(Result::ok)
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        }
    };
    let Some(banner) = run(
        vec!["--version".to_owned()],
        &super::compositor_binary(compositor),
    )
    .await
    else {
        return;
    };
    let Some(version) = shell_version_from_banner(&banner) else {
        return;
    };
    let current = run(
        vec![
            "get".into(),
            WELCOME_DIALOG_SCHEMA.into(),
            WELCOME_DIALOG_KEY.into(),
        ],
        GSETTINGS,
    )
    .await
    .unwrap_or_default();
    let current = current.trim().trim_matches('\'');
    if !welcome_marker_needs_update(version, current) {
        return;
    }
    let set = run(
        vec![
            "set".into(),
            WELCOME_DIALOG_SCHEMA.into(),
            WELCOME_DIALOG_KEY.into(),
            format!("'{version}'"),
        ],
        GSETTINGS,
    )
    .await;
    if set.is_some() {
        tracing::info!(target: target::SESSION, version, "first login: skipped by policy; the shell's welcome-tour marker raised");
    } else {
        tracing::warn!(target: target::SESSION, "first login: the welcome-tour marker could not be set");
    }
}
