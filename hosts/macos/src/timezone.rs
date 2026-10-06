//! macOS session-scoped time-zone redirection.
//!
//! The macOS Pier has exactly one helper: the launchd-managed per-session
//! Agent Helper. Time-zone redirection is enabled only in that Aqua LaunchAgent
//! job (`XPC_SERVICE_NAME=pier.arcen.tech.agent`) running as the signed-in user.
//! launchd runs exactly one such job per GUI domain, so this process is the
//! single writer for Arcen's `TZ` variables. A same-uid manual process has no
//! extra privilege over that user's environment and is intentionally out of
//! scope.
//!
//! State lives only in that user's GUI launchd domain: `ARCEN_SESSION_TZ` marks
//! values Arcen set and disappears with the session.

#![allow(unsafe_code)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use arcen_session::zoneinfo::{ZoneinfoValidationError, validate_zoneinfo_timezone};

const DEFAULT_ZONEINFO_ROOT: &str = "/var/db/timezone/zoneinfo";
const LAUNCHCTL: &str = "/bin/launchctl";
const SESSION_SENTINEL: &str = "ARCEN_SESSION_TZ";
const SESSION_AGENT_XPC_SERVICE: &str = "pier.arcen.tech.agent";
const XPC_SERVICE_NAME: &str = "XPC_SERVICE_NAME";
const TZ: &str = "TZ";

static SESSION_TZ: Mutex<SessionTimezoneRuntime> = Mutex::new(SessionTimezoneRuntime {
    shutting_down: false,
});

/// Result of a session time-zone redirection request.
#[derive(Debug)]
pub enum SessionTimezoneOutcome {
    /// The feature is disabled in `pier.json`.
    Disabled,
    /// The Deck did not provide a time-zone identifier.
    Absent,
    /// The Deck provided a malformed identifier.
    Invalid(String),
    /// The identifier was syntactically valid but unavailable on this host.
    Unsupported(String),
    /// The LoginWindow/root agent cannot safely set a user's GUI environment.
    UnsupportedAtLoginWindow,
    /// This is not the launchd-managed session Agent Helper.
    UnsupportedNotSessionAgent,
    /// The user already had their own `TZ`; Arcen leaves it alone.
    UserDefinedTimezone,
    /// The agent is already shutting down and refused a new apply.
    ShuttingDown,
    /// `TZ` was applied and will be restored by the returned lease.
    Applied(SessionTimezoneLease),
    /// A nonfatal failure prevented redirection.
    Warning(String),
}

/// A stale Arcen-set timezone restored when an Agent Helper starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredSessionTimezone {
    /// Value removed or observed during recovery.
    pub target: String,
    /// Whether `TZ` was unset by recovery. False means another writer changed it.
    pub unset_tz: bool,
}

/// RAII guard that restores the GUI-domain `TZ` value when the session ends.
pub struct SessionTimezoneLease {
    target: String,
    active: bool,
    restore: Option<Box<dyn FnMut() -> Result<(), String> + Send>>,
}

impl std::fmt::Debug for SessionTimezoneLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionTimezoneLease")
            .field("target", &self.target)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl SessionTimezoneLease {
    /// Deck time-zone value applied to this launchd GUI domain.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Restores Arcen's session time-zone variables and consumes the lease.
    ///
    /// # Errors
    ///
    /// Returns an error when `launchctl` refuses the restore.
    pub fn finish(mut self) -> Result<(), String> {
        self.restore()
    }

    fn restore(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        if let Some(restore) = self.restore.as_mut() {
            restore()?;
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for SessionTimezoneLease {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::error!(
                target: arcen_telemetry::names::target::SESSION,
                %error,
                target_timezone = %self.target,
                "session timezone restore failed"
            );
        }
    }
}

#[derive(Debug)]
struct SessionTimezoneRuntime {
    shutting_down: bool,
}

trait LaunchctlEnv: Send {
    fn getenv(&mut self, name: &str) -> Result<Option<String>, String>;
    fn setenv(&mut self, name: &str, value: &str) -> Result<(), String>;
    fn unsetenv(&mut self, name: &str) -> Result<(), String>;
}

struct CommandLaunchctl;

impl CommandLaunchctl {
    #[cfg(not(test))]
    fn new() -> Self {
        Self
    }

    #[cfg(test)]
    fn new() -> Self {
        panic!("CommandLaunchctl must not be constructed by tests")
    }
}

impl LaunchctlEnv for CommandLaunchctl {
    fn getenv(&mut self, name: &str) -> Result<Option<String>, String> {
        let output = std::process::Command::new(LAUNCHCTL)
            .arg("getenv")
            .arg(name)
            .output()
            .map_err(|error| format!("run launchctl getenv {name}: {error}"))?;
        parse_launchctl_getenv_output(
            name,
            output.status.success(),
            &output.stdout,
            &output.stderr,
        )
    }

    fn setenv(&mut self, name: &str, value: &str) -> Result<(), String> {
        run_launchctl(["setenv", name, value])
    }

    fn unsetenv(&mut self, name: &str) -> Result<(), String> {
        run_launchctl(["unsetenv", name])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessSupport {
    Supported,
    LoginWindowOrRoot,
    NotSessionAgent,
}

/// Returns whether this process may mutate a user's Aqua launchd environment.
#[must_use]
pub fn supported_in_this_process(login_window: bool) -> bool {
    process_support(
        login_window,
        current_euid(),
        std::env::var(XPC_SERVICE_NAME).ok().as_deref(),
    ) == ProcessSupport::Supported
}

fn process_support(
    login_window: bool,
    euid: u32,
    xpc_service_name: Option<&str>,
) -> ProcessSupport {
    if login_window || euid == 0 {
        return ProcessSupport::LoginWindowOrRoot;
    }
    if xpc_service_name != Some(SESSION_AGENT_XPC_SERVICE) {
        return ProcessSupport::NotSessionAgent;
    }
    ProcessSupport::Supported
}

/// Restores any Arcen-set GUI-domain timezone left by a crashed agent.
///
/// # Errors
///
/// Returns an error when the GUI-domain variables cannot be read or restored.
pub fn restore_stale_session_timezone() -> Result<Option<RestoredSessionTimezone>, String> {
    ensure_supported_process()?;
    let mut runtime = SESSION_TZ
        .lock()
        .map_err(|_| "session timezone mutex is poisoned".to_owned())?;
    let env: Arc<Mutex<dyn LaunchctlEnv>> = Arc::new(Mutex::new(CommandLaunchctl::new()));
    restore_locked(&env, &mut runtime)
}

/// Restores the active lease while the agent is handling SIGTERM.
///
/// This sets the shutdown flag under the same mutex that guards apply, so an
/// in-flight apply either completes before restore or sees shutdown and refuses.
///
/// # Errors
///
/// Returns an error when launchd refuses the restore.
pub fn restore_active_session_timezone_for_shutdown() -> Result<(), String> {
    ensure_supported_process()?;
    let mut runtime = SESSION_TZ
        .lock()
        .map_err(|_| "session timezone mutex is poisoned".to_owned())?;
    runtime.shutting_down = true;
    let env: Arc<Mutex<dyn LaunchctlEnv>> = Arc::new(Mutex::new(CommandLaunchctl::new()));
    let _ = restore_locked(&env, &mut runtime)?;
    Ok(())
}

/// Begins session-scoped timezone redirection in this Agent Helper's launchd
/// GUI domain.
#[must_use]
pub fn begin_session_timezone(
    feature_enabled: bool,
    requested: Option<&str>,
    login_window: bool,
) -> SessionTimezoneOutcome {
    if !feature_enabled {
        return SessionTimezoneOutcome::Disabled;
    }
    match process_support(
        login_window,
        current_euid(),
        std::env::var(XPC_SERVICE_NAME).ok().as_deref(),
    ) {
        ProcessSupport::Supported => {}
        ProcessSupport::LoginWindowOrRoot => {
            return SessionTimezoneOutcome::UnsupportedAtLoginWindow;
        }
        ProcessSupport::NotSessionAgent => {
            return SessionTimezoneOutcome::UnsupportedNotSessionAgent;
        }
    }
    let Some(requested) = requested else {
        return SessionTimezoneOutcome::Absent;
    };
    let timezone = match validate_zoneinfo_timezone(Path::new(DEFAULT_ZONEINFO_ROOT), requested) {
        Ok(timezone) => timezone,
        Err(ZoneinfoValidationError::InvalidIdentifier) => {
            return SessionTimezoneOutcome::Invalid("invalid IANA time-zone identifier".to_owned());
        }
        Err(
            ZoneinfoValidationError::EntryUnavailable | ZoneinfoValidationError::NotRegularFile,
        ) => return SessionTimezoneOutcome::Unsupported(requested.to_owned()),
        Err(error) => return SessionTimezoneOutcome::Warning(error.to_string()),
    };
    match begin_validated(timezone.as_str()) {
        Ok(outcome) => outcome,
        Err(error) => SessionTimezoneOutcome::Warning(error),
    }
}

fn begin_validated(timezone: &str) -> Result<SessionTimezoneOutcome, String> {
    let mut runtime = SESSION_TZ
        .lock()
        .map_err(|_| "session timezone mutex is poisoned".to_owned())?;
    let env: Arc<Mutex<dyn LaunchctlEnv>> = Arc::new(Mutex::new(CommandLaunchctl::new()));
    apply_locked(&env, &mut runtime, timezone)
}

fn apply_locked(
    env: &Arc<Mutex<dyn LaunchctlEnv>>,
    runtime: &mut SessionTimezoneRuntime,
    timezone: &str,
) -> Result<SessionTimezoneOutcome, String> {
    if runtime.shutting_down {
        return Ok(SessionTimezoneOutcome::ShuttingDown);
    }
    let mut env_guard = env
        .lock()
        .map_err(|_| "session timezone launchctl mutex is poisoned".to_owned())?;
    let sentinel = env_guard.getenv(SESSION_SENTINEL)?;
    let current_tz = env_guard.getenv(TZ)?;
    if sentinel.is_none() && current_tz.is_some() {
        return Ok(SessionTimezoneOutcome::UserDefinedTimezone);
    }
    env_guard.setenv(SESSION_SENTINEL, timezone)?;
    if let Err(error) = env_guard.setenv(TZ, timezone) {
        let _ = env_guard.unsetenv(SESSION_SENTINEL);
        return Err(error);
    }
    let env = Arc::clone(env);
    Ok(SessionTimezoneOutcome::Applied(SessionTimezoneLease {
        target: timezone.to_owned(),
        active: true,
        restore: Some(Box::new(move || {
            let mut runtime = SESSION_TZ
                .lock()
                .map_err(|_| "session timezone mutex is poisoned".to_owned())?;
            let _ = restore_locked(&env, &mut runtime)?;
            Ok(())
        })),
    }))
}

fn restore_locked(
    env: &Arc<Mutex<dyn LaunchctlEnv>>,
    _runtime: &mut SessionTimezoneRuntime,
) -> Result<Option<RestoredSessionTimezone>, String> {
    let mut env = env
        .lock()
        .map_err(|_| "session timezone launchctl mutex is poisoned".to_owned())?;
    let Some(sentinel) = env.getenv(SESSION_SENTINEL)? else {
        return Ok(None);
    };
    let current_tz = env.getenv(TZ)?;
    let unset_tz = current_tz.as_deref() == Some(sentinel.as_str());
    if unset_tz {
        env.unsetenv(TZ)?;
    } else if let Some(current) = current_tz.as_deref() {
        tracing::warn!(
            target: arcen_telemetry::names::target::SESSION,
            current_timezone = %current,
            arcen_timezone = %sentinel,
            "not unsetting TZ because it changed outside Arcen"
        );
    }
    env.unsetenv(SESSION_SENTINEL)?;
    Ok(Some(RestoredSessionTimezone {
        target: sentinel,
        unset_tz,
    }))
}

fn parse_launchctl_getenv_output(
    name: &str,
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<String>, String> {
    if !success {
        let stderr = String::from_utf8_lossy(stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("launchctl getenv {name} failed")
        } else {
            format!("launchctl getenv {name}: {stderr}")
        });
    }
    if stdout.is_empty() {
        return Ok(None);
    }
    let value = stdout.strip_suffix(b"\n").unwrap_or(stdout);
    String::from_utf8(value.to_vec())
        .map(Some)
        .map_err(|error| format!("launchctl getenv {name} returned non-UTF-8 output: {error}"))
}

fn run_launchctl<const N: usize>(args: [&str; N]) -> Result<(), String> {
    let output = std::process::Command::new(LAUNCHCTL)
        .args(args)
        .output()
        .map_err(|error| format!("run launchctl: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        Err(format!("launchctl exited with {}", output.status))
    } else {
        Err(format!("launchctl: {stderr}"))
    }
}

fn ensure_supported_process() -> Result<(), String> {
    match process_support(
        false,
        current_euid(),
        std::env::var(XPC_SERVICE_NAME).ok().as_deref(),
    ) {
        ProcessSupport::Supported => Ok(()),
        ProcessSupport::LoginWindowOrRoot => {
            Err("session timezone redirection is unsupported in a root agent".to_owned())
        }
        ProcessSupport::NotSessionAgent => Err(
            "session timezone redirection is supported only in the launchd Agent Helper".to_owned(),
        ),
    }
}

fn current_euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not dereference pointers.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeState {
        vars: BTreeMap<String, String>,
        operations: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct SharedFake {
        state: Arc<Mutex<FakeState>>,
    }

    impl SharedFake {
        fn with(name: &str, value: &str) -> Self {
            let fake = Self::default();
            fake.state
                .lock()
                .unwrap()
                .vars
                .insert(name.to_owned(), value.to_owned());
            fake
        }

        fn env(&self) -> Arc<Mutex<dyn LaunchctlEnv>> {
            Arc::new(Mutex::new(self.clone()))
        }

        fn set_var(&self, name: &str, value: &str) {
            self.state
                .lock()
                .unwrap()
                .vars
                .insert(name.to_owned(), value.to_owned());
        }

        fn var(&self, name: &str) -> Option<String> {
            self.state.lock().unwrap().vars.get(name).cloned()
        }

        fn operations(&self) -> Vec<String> {
            self.state.lock().unwrap().operations.clone()
        }
    }

    impl LaunchctlEnv for SharedFake {
        fn getenv(&mut self, name: &str) -> Result<Option<String>, String> {
            Ok(self.state.lock().unwrap().vars.get(name).cloned())
        }

        fn setenv(&mut self, name: &str, value: &str) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.operations.push(format!("set:{name}={value}"));
            state.vars.insert(name.to_owned(), value.to_owned());
            Ok(())
        }

        fn unsetenv(&mut self, name: &str) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.operations.push(format!("unset:{name}"));
            state.vars.remove(name);
            Ok(())
        }
    }

    fn runtime() -> SessionTimezoneRuntime {
        SessionTimezoneRuntime {
            shutting_down: false,
        }
    }

    #[test]
    fn gate_requires_launchd_agent_non_root_aqua() {
        assert_eq!(
            process_support(false, 501, Some(SESSION_AGENT_XPC_SERVICE)),
            ProcessSupport::Supported
        );
        assert_eq!(
            process_support(false, 501, None),
            ProcessSupport::NotSessionAgent
        );
        assert_eq!(
            process_support(false, 501, Some("manual")),
            ProcessSupport::NotSessionAgent
        );
        assert_eq!(
            process_support(false, 0, Some(SESSION_AGENT_XPC_SERVICE)),
            ProcessSupport::LoginWindowOrRoot
        );
        assert_eq!(
            process_support(true, 501, Some(SESSION_AGENT_XPC_SERVICE)),
            ProcessSupport::LoginWindowOrRoot
        );
    }

    #[test]
    fn apply_sets_sentinel_before_tz() {
        let fake = SharedFake::default();
        let env = fake.env();
        let mut runtime = runtime();
        let SessionTimezoneOutcome::Applied(lease) =
            apply_locked(&env, &mut runtime, "Asia/Tokyo").unwrap()
        else {
            panic!("expected applied lease");
        };
        assert_eq!(
            fake.operations(),
            ["set:ARCEN_SESSION_TZ=Asia/Tokyo", "set:TZ=Asia/Tokyo"]
        );
        std::mem::forget(lease);
    }

    #[test]
    fn lease_drop_uses_injected_backend() {
        let fake = SharedFake::default();
        let env = fake.env();
        let mut runtime = runtime();
        let SessionTimezoneOutcome::Applied(lease) =
            apply_locked(&env, &mut runtime, "Asia/Tokyo").unwrap()
        else {
            panic!("expected applied lease");
        };
        drop(lease);
        assert_eq!(fake.var(TZ), None);
        assert_eq!(fake.var(SESSION_SENTINEL), None);
    }

    #[test]
    fn apply_refuses_user_defined_timezone() {
        let fake = SharedFake::with(TZ, "Europe/Oslo");
        let env = fake.env();
        let mut runtime = runtime();
        assert!(matches!(
            apply_locked(&env, &mut runtime, "Asia/Tokyo").unwrap(),
            SessionTimezoneOutcome::UserDefinedTimezone
        ));
        assert!(fake.operations().is_empty());
    }

    #[test]
    fn apply_after_shutdown_is_refused() {
        let fake = SharedFake::default();
        let env = fake.env();
        let mut runtime = SessionTimezoneRuntime {
            shutting_down: true,
        };
        assert!(matches!(
            apply_locked(&env, &mut runtime, "Asia/Tokyo").unwrap(),
            SessionTimezoneOutcome::ShuttingDown
        ));
        assert!(fake.operations().is_empty());
    }

    #[test]
    fn restore_unsets_tz_only_when_it_still_matches_sentinel() {
        let fake = SharedFake::default();
        fake.set_var(SESSION_SENTINEL, "Asia/Tokyo");
        fake.set_var(TZ, "Asia/Tokyo");
        let env = fake.env();
        let mut runtime = runtime();
        let restored = restore_locked(&env, &mut runtime).unwrap().unwrap();
        assert!(restored.unset_tz);
        assert_eq!(fake.var(TZ), None);
        assert_eq!(fake.var(SESSION_SENTINEL), None);
        assert_eq!(fake.operations(), ["unset:TZ", "unset:ARCEN_SESSION_TZ"]);
    }

    #[test]
    fn restore_leaves_externally_changed_tz() {
        let fake = SharedFake::default();
        fake.set_var(SESSION_SENTINEL, "Asia/Tokyo");
        fake.set_var(TZ, "Europe/Oslo");
        let env = fake.env();
        let mut runtime = runtime();
        let restored = restore_locked(&env, &mut runtime).unwrap().unwrap();
        assert!(!restored.unset_tz);
        assert_eq!(fake.var(TZ).as_deref(), Some("Europe/Oslo"));
        assert_eq!(fake.var(SESSION_SENTINEL), None);
        assert_eq!(fake.operations(), ["unset:ARCEN_SESSION_TZ"]);
    }

    #[test]
    fn restore_without_sentinel_is_noop() {
        let fake = SharedFake::with(TZ, "Europe/Oslo");
        let env = fake.env();
        let mut runtime = runtime();
        assert_eq!(restore_locked(&env, &mut runtime).unwrap(), None);
        assert_eq!(fake.var(TZ).as_deref(), Some("Europe/Oslo"));
        assert!(fake.operations().is_empty());
    }

    #[test]
    fn launchctl_getenv_distinguishes_absent_empty_and_newlines() {
        assert_eq!(
            parse_launchctl_getenv_output(TZ, true, b"", b"").unwrap(),
            None
        );
        assert_eq!(
            parse_launchctl_getenv_output(TZ, true, b"\n", b"").unwrap(),
            Some(String::new())
        );
        assert_eq!(
            parse_launchctl_getenv_output(TZ, true, b"Asia/Tokyo\n", b"").unwrap(),
            Some("Asia/Tokyo".to_owned())
        );
        assert_eq!(
            parse_launchctl_getenv_output(TZ, true, b"Asia/Tokyo\n\n", b"").unwrap(),
            Some("Asia/Tokyo\n".to_owned())
        );
        assert!(parse_launchctl_getenv_output(TZ, false, b"", b"nope").is_err());
    }

    #[test]
    fn login_window_reports_timezone_unsupported_without_touching_zoneinfo() {
        assert!(matches!(
            begin_session_timezone(true, Some("Asia/Tokyo"), true),
            SessionTimezoneOutcome::UnsupportedAtLoginWindow
        ));
    }
}
