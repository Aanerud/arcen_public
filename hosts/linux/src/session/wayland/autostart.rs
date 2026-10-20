//! The settings daemons and XDG autostart programs of a headless session.
//!
//! Reads the autostart directories, decides with the shared policy
//! ([`arcen_session::desktop_session::autostart`] and
//! [`arcen_session::desktop_session::components`]) what runs and when, and
//! supervises what it started: every program is an optional member (its
//! exit never ends the desktop), `X-GNOME-AutoRestart` entries restart
//! within a budget, and everything stops before the compositor at
//! teardown. Programs run as the session user in the session's scope with
//! the stack's environment, so the scope sweep covers anything they leave.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use arcen_session::desktop_session::autostart::{
    merge, should_start, AutostartEntry, Condition, EntryFacts, Merged, Stage,
};
use arcen_session::desktop_session::components::{classify, keyring_components, ComponentVerdict};
use arcen_session::desktop_session::restart::{AutoRestartBudget, RestartDecision};
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::stack::MemberEnvironment;
use crate::logging::target;

const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(250);
const GSETTINGS: &str = "/usr/bin/gsettings";
const SETTINGS_TIMEOUT: Duration = Duration::from_secs(2);
const SYSTEM_CONFIG_DIRS: &str = "/etc/xdg";

/// The autostart directories, highest priority first:
/// `$XDG_CONFIG_HOME/autostart`, then each `$XDG_CONFIG_DIRS/autostart`.
#[must_use]
pub fn autostart_directories(config_home: &Path, config_dirs: Option<&str>) -> Vec<PathBuf> {
    let mut directories = vec![config_home.join("autostart")];
    let system = config_dirs
        .filter(|dirs| !dirs.trim().is_empty())
        .unwrap_or(SYSTEM_CONFIG_DIRS);
    for directory in system.split(':').filter(|dir| dir.starts_with('/')) {
        let directory = Path::new(directory).join("autostart");
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    directories
}

/// The user's configuration directory as the session sees it:
/// `$XDG_CONFIG_HOME` from the session environment (the login shell may set
/// it), else `$HOME/.config`.
#[must_use]
pub fn config_home(members: &MemberEnvironment) -> PathBuf {
    resolve_config_home(members.var("XDG_CONFIG_HOME"), members.var("HOME"))
}

fn resolve_config_home(config_home: Option<String>, home: Option<String>) -> PathBuf {
    config_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/nonexistent/.config"))
}

/// Reads the `.desktop` files of `directories` (bounded; symlinks only
/// within the directory).
#[must_use]
pub fn read_autostart(directories: &[PathBuf]) -> Merged {
    use arcen_session::desktop_session::autostart::{
        desktop_file_id, MAX_ENTRIES, MAX_ENTRY_BYTES,
    };
    let mut all = Vec::new();
    for directory in directories {
        let mut files = Vec::new();
        let Ok(entries) = std::fs::read_dir(directory) else {
            all.push(files);
            continue;
        };
        for entry in entries.flatten().take(MAX_ENTRIES) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if desktop_file_id(&name).is_none() {
                continue;
            }
            // Entries are commonly symlinks into /usr/share/applications;
            // they are read as the session user, like any other file of
            // theirs.
            let target = entry.path();
            let Ok(metadata) = std::fs::metadata(&target) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_ENTRY_BYTES as u64 {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&target) {
                files.push((name, bytes));
            }
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        all.push(files);
    }
    merge(&all)
}

/// The decided plan of one session.
#[derive(Debug, Default)]
pub struct AutostartPlan {
    /// Entries to run, with their verdict (`Run` or `AfterX11Display`).
    pub run: Vec<(AutostartEntry, ComponentVerdict)>,
    /// The keyring components Arcen starts itself.
    pub keyring_components: Vec<String>,
}

/// Decides the plan for `merged` (conditions evaluated now).
pub async fn plan(
    merged: Merged,
    members: &MemberEnvironment,
    current_desktop: &str,
) -> AutostartPlan {
    for (id, error) in &merged.invalid {
        tracing::info!(target: target::SESSION, id, reason = %error, "autostart entry ignored: unreadable");
    }
    for id in &merged.deleted {
        tracing::info!(target: target::SESSION, id, "autostart entry hidden by a higher-priority file");
    }
    let facts = Facts::gather(&merged.entries, members).await;
    let mut startable = Vec::new();
    for entry in merged.entries {
        match should_start(&entry, current_desktop, &facts) {
            Ok(()) => startable.push(entry),
            Err(reason) => tracing::info!(
                target: target::SESSION,
                id = entry.id,
                reason = reason.token(),
                "autostart entry not started"
            ),
        }
    }
    let keyring = keyring_components(&startable);
    let mut run = Vec::new();
    for entry in startable {
        let verdict = classify(&entry);
        match verdict {
            ComponentVerdict::Run | ComponentVerdict::AfterX11Display => run.push((entry, verdict)),
            ComponentVerdict::ProvidedByArcen(_) | ComponentVerdict::HostScoped(_) => {
                tracing::info!(
                    target: target::SESSION,
                    id = entry.id,
                    verdict = verdict.token(),
                    "autostart entry not started by the supervisor"
                );
            }
        }
    }
    run.sort_by(|(left, _), (right, _)| left.phase.cmp(&right.phase).then(left.id.cmp(&right.id)));
    AutostartPlan {
        run,
        keyring_components: keyring,
    }
}

struct Facts {
    config_home: PathBuf,
    path: Vec<PathBuf>,
    settings: BTreeMap<(String, String), bool>,
}

impl Facts {
    async fn gather(entries: &[AutostartEntry], members: &MemberEnvironment) -> Self {
        let mut wanted = BTreeSet::new();
        for entry in entries {
            if let Some(Condition::GSettings { schema, key }) = &entry.condition {
                wanted.insert((schema.clone(), key.clone()));
            }
        }
        let mut settings = BTreeMap::new();
        for (schema, key) in wanted {
            if let Some(value) = read_boolean(members, &schema, &key).await {
                settings.insert((schema, key), value);
            }
        }
        Self {
            config_home: config_home(members),
            path: members
                .path()
                .split(':')
                .filter(|dir| dir.starts_with('/'))
                .map(PathBuf::from)
                .collect(),
            settings,
        }
    }
}

impl EntryFacts for Facts {
    fn config_path_exists(&self, relative: &str) -> bool {
        self.config_home.join(relative).exists()
    }

    fn settings_boolean(&self, schema: &str, key: &str) -> Option<bool> {
        self.settings
            .get(&(schema.to_owned(), key.to_owned()))
            .copied()
    }

    fn program_exists(&self, program: &str) -> bool {
        resolve_program(program, &self.path).is_some()
    }

    fn session_name(&self) -> &'static str {
        "gnome"
    }
}

/// `program` itself when absolute, else the first executable match on
/// `path`.
fn resolve_program(program: &str, path: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |candidate: &Path| {
        std::fs::metadata(candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return (candidate.is_absolute() && executable(&candidate)).then_some(candidate);
    }
    path.iter()
        .map(|dir| dir.join(program))
        .find(|candidate| executable(candidate))
}

async fn read_boolean(members: &MemberEnvironment, schema: &str, key: &str) -> Option<bool> {
    let mut command = members.command(GSETTINGS);
    command
        .args(["get", schema, key])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = tokio::time::timeout(SETTINGS_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The running supervisor.
pub struct AutostartSupervisor {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AutostartSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AutostartSupervisor")
            .finish_non_exhaustive()
    }
}

impl AutostartSupervisor {
    /// Starts supervising `plan`: initialization-stage entries now,
    /// application-stage entries once `ready` turns true, X11-dependent
    /// entries once `environment` holds `DISPLAY`.
    #[must_use]
    pub fn start(
        plan: AutostartPlan,
        members: MemberEnvironment,
        ready: watch::Receiver<bool>,
        environment: watch::Receiver<BTreeMap<String, String>>,
        session_id: &str,
    ) -> Self {
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(
            plan.run,
            members,
            ready,
            environment,
            stop_rx,
            session_id.to_owned(),
        ));
        Self {
            stop,
            task: Some(task),
        }
    }

    /// Stops every supervised program (SIGTERM, then SIGKILL), bounded.
    pub async fn stop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            if tokio::time::timeout(STOP_TIMEOUT + Duration::from_secs(1), task)
                .await
                .is_err()
            {
                tracing::warn!(target: target::SESSION, "autostart supervisor did not stop in time");
            }
        }
    }
}

impl Drop for AutostartSupervisor {
    fn drop(&mut self) {
        // Aborting drops the children, which are killed on drop.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Supervised {
    entry: AutostartEntry,
    verdict: ComponentVerdict,
    child: Option<Child>,
    budget: AutoRestartBudget,
    restart_at: Option<tokio::time::Instant>,
    start_at: Option<tokio::time::Instant>,
    given_up: bool,
    startup_id: String,
}

async fn supervise(
    plan: Vec<(AutostartEntry, ComponentVerdict)>,
    members: MemberEnvironment,
    mut ready: watch::Receiver<bool>,
    mut environment: watch::Receiver<BTreeMap<String, String>>,
    mut stop: watch::Receiver<bool>,
    session_id: String,
) {
    let epoch = tokio::time::Instant::now();
    let mut supervised = plan
        .into_iter()
        .enumerate()
        .map(|(index, (entry, verdict))| Supervised {
            startup_id: format!("arcen-{session_id}-{}", index + 1),
            entry,
            verdict,
            child: None,
            budget: AutoRestartBudget::default(),
            restart_at: None,
            start_at: None,
            given_up: false,
        })
        .collect::<Vec<_>>();
    let mut application_started = false;
    let mut x11_started = false;
    schedule(&mut supervised, Stage::Initialization, false);
    loop {
        let now = tokio::time::Instant::now();
        if !application_started && *ready.borrow() {
            application_started = true;
            schedule(&mut supervised, Stage::Application, false);
        }
        let display = environment.borrow().get("DISPLAY").cloned();
        if !x11_started && display.is_some() {
            x11_started = true;
            schedule(&mut supervised, Stage::Initialization, true);
        }
        let extra = environment.borrow().clone();
        for item in &mut supervised {
            let due = item.start_at.is_some_and(|at| at <= now)
                || item.restart_at.is_some_and(|at| at <= now);
            if due && item.child.is_none() && !item.given_up {
                item.start_at = None;
                item.restart_at = None;
                item.child = launch(item, &members, &extra);
            }
            if let Some(child) = item.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    item.child = None;
                    tracing::info!(target: target::SESSION, id = item.entry.id, %status, "autostart program exited");
                    if item.entry.auto_restart {
                        match item.budget.on_exit(now.duration_since(epoch)) {
                            RestartDecision::RestartAfter(delay) => {
                                item.restart_at = Some(now + delay);
                            }
                            RestartDecision::GiveUp => {
                                item.given_up = true;
                                tracing::warn!(target: target::SESSION, id = item.entry.id, "autostart program failed too often; not restarting it");
                            }
                        }
                    }
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(POLL) => {}
            () = changed_or_pending(&mut ready) => {}
            () = changed_or_pending(&mut environment) => {}
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
        }
    }
    stop_all(&mut supervised).await;
}

/// Resolves when `receiver` changes; never when its sender is gone.
async fn changed_or_pending<T>(receiver: &mut watch::Receiver<T>) {
    if receiver.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Marks the entries of `stage` (only X11-dependent ones when `x11`) due.
fn schedule(supervised: &mut [Supervised], stage: Stage, x11: bool) {
    let now = tokio::time::Instant::now();
    for item in supervised.iter_mut() {
        let wants_x11 = item.verdict == ComponentVerdict::AfterX11Display;
        if item.entry.phase.stage() == stage && wants_x11 == x11 {
            item.start_at = Some(now + Duration::from_secs(u64::from(item.entry.delay_secs)));
        }
    }
}

fn launch(
    item: &Supervised,
    members: &MemberEnvironment,
    extra: &BTreeMap<String, String>,
) -> Option<Child> {
    let (program, arguments) = item.entry.argv.split_first()?;
    let mut command = members.command(program);
    command
        .args(arguments)
        .envs(extra.iter())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(home) = std::env::var_os("HOME") {
        command.current_dir(home);
    }
    if item.entry.notify {
        command.env("DESKTOP_AUTOSTART_ID", &item.startup_id);
    }
    match command.spawn() {
        Ok(child) => {
            tracing::info!(
                target: target::SESSION,
                id = item.entry.id,
                pid = child.id().unwrap_or_default(),
                phase = item.entry.phase.token(),
                verdict = item.verdict.token(),
                "autostart program started"
            );
            Some(child)
        }
        Err(error) => {
            tracing::warn!(target: target::SESSION, id = item.entry.id, %error, "autostart program could not start");
            None
        }
    }
}

async fn stop_all(supervised: &mut [Supervised]) {
    for item in supervised.iter_mut() {
        if let Some(pid) = item
            .child
            .as_ref()
            .and_then(Child::id)
            .and_then(|pid| i32::try_from(pid).ok())
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    for item in supervised.iter_mut() {
        if let Some(child) = item.child.as_mut() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if tokio::time::timeout(remaining, child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        }
        item.child = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_home_follows_the_session_environment() {
        assert_eq!(
            resolve_config_home(Some("/srv/u/cfg".into()), Some("/home/u".into())),
            PathBuf::from("/srv/u/cfg")
        );
        assert_eq!(
            resolve_config_home(Some("relative".into()), Some("/home/u".into())),
            PathBuf::from("/home/u/.config")
        );
        assert_eq!(
            resolve_config_home(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.config")
        );
        let mut members = MemberEnvironment::new(Path::new("/run/user/1000/arcen-wayland-7"));
        members.set("XDG_CONFIG_HOME", "/srv/u/cfg");
        assert_eq!(config_home(&members), PathBuf::from("/srv/u/cfg"));
        assert_eq!(
            autostart_directories(&config_home(&members), Some("/opt/site/xdg")),
            [
                PathBuf::from("/srv/u/cfg/autostart"),
                PathBuf::from("/opt/site/xdg/autostart")
            ]
        );
    }

    #[test]
    fn autostart_directories_follow_xdg_order_without_duplicates() {
        assert_eq!(
            autostart_directories(Path::new("/home/u/.config"), None),
            [
                PathBuf::from("/home/u/.config/autostart"),
                PathBuf::from("/etc/xdg/autostart")
            ]
        );
        assert_eq!(
            autostart_directories(
                Path::new("/home/u/.config"),
                Some("/etc/xdg/xdg-gnome:/etc/xdg:relative:/etc/xdg")
            ),
            [
                PathBuf::from("/home/u/.config/autostart"),
                PathBuf::from("/etc/xdg/xdg-gnome/autostart"),
                PathBuf::from("/etc/xdg/autostart")
            ]
        );
    }

    #[test]
    fn programs_resolve_on_path_or_as_absolute_executables() {
        let path = [
            PathBuf::from("/nonexistent"),
            PathBuf::from("/bin"),
            PathBuf::from("/usr/bin"),
        ];
        assert!(resolve_program("sh", &path).is_some());
        assert!(resolve_program("/bin/sh", &[]).is_some());
        assert!(resolve_program("bin/sh", &path).is_none());
        assert!(resolve_program("no-such-program-arcen", &path).is_none());
        assert!(resolve_program("/etc/hostname", &[]).is_none());
    }
}
