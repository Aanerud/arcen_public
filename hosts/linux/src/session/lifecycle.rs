//! Persistent single-user graphical desktop lifecycle.

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use crate::display::nvctrl::MetaModeGuard;
use crate::display::topology::LinuxTopologyPlan;

use arcen_protocol::messages::MultiMonitorCarrierMsg;
#[cfg(test)]
use arcen_session::restore_lease::RestorePhase;
use arcen_session::restore_lease::{
    IanaTimeZone, LeaseOwnerId, RestoreEvent, RestoreLease, RestoreResource, StateFingerprint,
};
use arcen_telemetry::CorrelationId;
use thiserror::Error;
use tokio::sync::{watch, OwnedSemaphorePermit};
use tokio::task::JoinHandle;

use super::identity::UserExecution;
use super::launcher::{
    validate_active_logind_session, AuthenticatedLauncher, LauncherError, SessionLauncher,
};
use super::resume::{ResumeRegistry, ResumeRegistryError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    pub username: String,
    pub uid: u32,
    pub session_id: String,
    pub session_type: String,
    pub desktop: String,
    pub display: String,
    pub agent_pid: u32,
    pub generation: u64,
    pub reconnected: bool,
    pub timezone: Option<IanaTimeZone>,
    /// The `multi_monitor_v1` topology committed for this persistent desktop
    /// at `Create` time, or `None` for every session that never requested
    /// (or was never admitted for) multi-monitor — including every session
    /// while the operator's own `--multi-monitor` gate stays off, which
    /// remains this host's default and the sole production safety switch
    /// (`media::multi_capenc::MULTI_MONITOR_CARRIER_READY` is `true`).
    ///
    /// Fixed for the lifetime of the desktop: a `Reconnect` always carries
    /// forward the *original* `Create`'s plan (see
    /// [`SessionRegistry::acquire`]) rather than whatever topology the
    /// reconnecting attempt itself just computed, matching this tranche's
    /// fixed-topology/no-live-renegotiation contract.
    pub multi_monitor_plan: Option<LinuxTopologyPlan>,
    /// The carrier committed alongside [`Self::multi_monitor_plan`], or
    /// `None` exactly when `multi_monitor_plan` is `None`.
    pub multi_monitor_carrier: Option<MultiMonitorCarrierMsg>,
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("desktop session is busy")]
    Busy,
    #[error("session launcher failed: {0}")]
    Launcher(#[from] LauncherError),
    #[error("timezone restore lease could not be constructed or transitioned")]
    TimezoneLease,
    #[error("direct resume authority could not be initialized")]
    ResumeAuthority,
    #[error("authenticated native session changed")]
    NativeSessionChanged,
    #[error("display ownership is unavailable")]
    DisplayOwnership,
}

/// Durable, monotonic latch tracking whether *this* persistent desktop's
/// currently committed multi-monitor plan has ever reached a usable
/// attachment (see `AttachmentEnd::reached_usable` in `net::server`),
/// across any number of past attachments/reconnects since the plan was
/// last (re)committed at `Create`.
///
/// Lives on [`DesktopSession`] — a brand-new instance (`false`) is
/// constructed exactly once per fresh `Create` (never on `Reconnect`,
/// which reuses the existing `DesktopSession` untouched), and the whole
/// value is dropped with the `DesktopSession` when the desktop is
/// terminated/removed. So "reset on new desktop/plan" and "session
/// removal resets" both fall out of construction alone: there is no
/// explicit reset path, only ever constructing a fresh, unproven `false`
/// instance for a fresh plan.
///
/// Deliberately just a plain monotonic OR-latch: [`Self::mark`] can only
/// ever move `false` -> `true`, never back. Combined with
/// [`SessionRegistry::mark_multi_monitor_ever_usable`]'s
/// generation/username match guard (which makes a stale/late-arriving
/// mark for an already-replaced desktop a no-op rather than mutating the
/// wrong generation's state), concurrent attachments and cleanup can
/// never regress an already-proven-usable desktop back to unproven.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MultiMonitorEverUsable(bool);

impl MultiMonitorEverUsable {
    const fn new() -> Self {
        Self(false)
    }

    fn mark(&mut self) {
        self.0 = true;
    }

    const fn get(self) -> bool {
        self.0
    }
}

/// Whether a reconnect to a disconnected desktop must replace it because the
/// current client layout differs from the desktop's committed topology.
///
/// A desktop's committed topology is fixed for its whole lifetime (see
/// [`SessionMetadata::multi_monitor_plan`]): a desktop created by a
/// single-primary or capability-probe connect commits `None`, and every
/// later Match My Layout connect reattaches to that same `None`. ADR-0009
/// forbids silently serving the primary-only subset, so the only remaining
/// Match My Layout is current-layout authoritative: moving between locations,
/// adding/removing displays, or switching to Primary Display Only must create
/// a desktop matching that request instead of reattaching a stale roster.
///
/// Extracted to exactly the three inputs the decision depends on so it is
/// directly unit-testable without a live registry.
fn reconnect_must_replace_desktop(topology_matches: bool) -> bool {
    !topology_matches
}

/// Whether a disconnected persistent desktop has exceeded the operator
/// configured idle lifetime and should be torn down before serving a new
/// connection.
///
/// The clock starts only when the desktop becomes disconnected, not when it
/// is created: connected desktops are always kept, and an absent configured
/// limit preserves the historical persistent-desktop behaviour exactly. At
/// the exact boundary the desktop is still retained; reaping starts only
/// once the disconnected age is greater than the limit.
///
/// Extracted to exactly the state this policy depends on so it is directly
/// unit-testable without a live registry, Xorg, systemd, or launcher process.
fn disconnected_idle_lifetime_expired(
    connected: bool,
    disconnected_for: Option<Duration>,
    configured_limit: Option<Duration>,
) -> bool {
    if connected {
        return false;
    }
    match (disconnected_for, configured_limit) {
        (Some(disconnected_for), Some(configured_limit)) => disconnected_for > configured_limit,
        _ => false,
    }
}

struct DesktopSession {
    launcher: SessionLauncher,
    connected: bool,
    disconnected_since: Option<SystemTime>,
    generation: u64,
    created_at: SystemTime,
    timezone: Option<IanaTimeZone>,
    timezone_lease: Option<RestoreLease>,
    held_display: Option<HeldDisplayResources>,
    /// See [`SessionMetadata::multi_monitor_plan`]. Committed once at
    /// `Create` and never mutated afterwards.
    multi_monitor_plan: Option<LinuxTopologyPlan>,
    multi_monitor_carrier: Option<MultiMonitorCarrierMsg>,
    /// See [`MultiMonitorEverUsable`]. Always starts `false` for a fresh
    /// `Create` (matching a freshly committed `multi_monitor_plan`), and
    /// is otherwise only ever latched to `true` via
    /// [`SessionRegistry::mark_multi_monitor_ever_usable`].
    multi_monitor_ever_usable: MultiMonitorEverUsable,
}

impl DesktopSession {
    fn metadata(&self, reconnected: bool) -> SessionMetadata {
        SessionMetadata {
            username: self.launcher.identity.username.clone(),
            uid: self.launcher.identity.uid,
            session_id: self
                .launcher
                .environment
                .session_id()
                .unwrap_or_default()
                .to_string(),
            session_type: self.launcher.environment.session_type().to_string(),
            desktop: self.launcher.environment.desktop().to_string(),
            display: self.launcher.environment.display().to_string(),
            agent_pid: self.launcher.agent_pid,
            generation: self.generation,
            reconnected,
            timezone: self.timezone.clone(),
            multi_monitor_plan: self.multi_monitor_plan.clone(),
            multi_monitor_carrier: self.multi_monitor_carrier,
        }
    }

    fn execution(&self) -> UserExecution {
        self.launcher.execution()
    }

    async fn shutdown(mut self) {
        let age_seconds = self
            .created_at
            .elapsed()
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut timezone_lease = self.timezone_lease.take();
        if let Some(mut held_display) = self.held_display.take() {
            held_display.restore().await;
        }
        let agent_exit_confirmed = self.launcher.shutdown().await;
        if complete_timezone_restore_after_shutdown(&mut timezone_lease, agent_exit_confirmed)
            .is_err()
        {
            tracing::warn!(
                target: crate::logging::target::SESSION,
                agent_exit_confirmed,
                "process timezone restore lease was not completed after desktop teardown"
            );
        }
        tracing::info!(
            target: crate::logging::target::SESSION,
            age_seconds,
            "persistent authenticated desktop launcher stopped"
        );
    }
}

pub struct SessionRegistry {
    inner: Mutex<RegistryState>,
    cleanup: Mutex<Option<CleanupTask>>,
    resume: Arc<ResumeRegistry>,
    disconnected_idle_lifetime: Option<Duration>,
}

#[derive(Default)]
struct RegistryState {
    session: Option<DesktopSession>,
    draining: bool,
}

struct CleanupTask {
    handle: JoinHandle<()>,
    completed: watch::Receiver<bool>,
}

struct CleanupCompletion {
    registry: Weak<SessionRegistry>,
    completed: Option<watch::Sender<bool>>,
}

impl Drop for CleanupCompletion {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.inner.lock().unwrap().draining = false;
        }
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(true);
        }
    }
}

impl SessionRegistry {
    pub(crate) fn new(
        disconnected_idle_lifetime: Option<Duration>,
    ) -> Result<Arc<Self>, ResumeRegistryError> {
        Ok(Arc::new(Self {
            inner: Mutex::new(RegistryState::default()),
            cleanup: Mutex::new(None),
            resume: ResumeRegistry::new()?,
            disconnected_idle_lifetime,
        }))
    }

    pub(crate) fn resume(&self) -> &ResumeRegistry {
        &self.resume
    }

    pub async fn acquire(
        self: &Arc<Self>,
        mut pending: AuthenticatedLauncher,
        multi_monitor: Option<(LinuxTopologyPlan, MultiMonitorCarrierMsg)>,
        _replace_incompatible_desktop: bool,
    ) -> Result<SessionLease, LifecycleError> {
        // Attached unconditionally, before this connection's fate (Create /
        // Reconnect / Busy / Dead-then-retry) is decided below: harmless
        // no-op whenever `pending` ends up discarded without `.open()`
        // (Reconnect/Busy/Dead), and the only source of truth this
        // connection has for what to persist on an actual `Create`.
        pending.set_multi_monitor_plan(multi_monitor.as_ref().map(|(plan, _carrier)| plan));
        let mut pending = Some(pending);
        loop {
            let decision = {
                let mut guard = self.inner.lock().unwrap();
                if guard.draining {
                    ExistingDecision::Draining
                } else {
                    let dead = guard
                        .session
                        .as_mut()
                        .is_some_and(|existing| !existing.launcher.is_running());
                    if dead {
                        guard.draining = true;
                        ExistingDecision::Dead(Box::new(
                            guard.session.take().expect("existing session"),
                        ))
                    } else if let Some((disconnected_for, configured_limit)) =
                        guard.session.as_ref().and_then(|existing| {
                            let disconnected_for = existing
                                .disconnected_since
                                .and_then(|since| since.elapsed().ok());
                            disconnected_idle_lifetime_expired(
                                existing.connected,
                                disconnected_for,
                                self.disconnected_idle_lifetime,
                            )
                            .then_some((disconnected_for?, self.disconnected_idle_lifetime?))
                        })
                    {
                        guard.draining = true;
                        ExistingDecision::IdleExpired(
                            Box::new(guard.session.take().expect("existing session")),
                            disconnected_for,
                            configured_limit,
                        )
                    } else {
                        match guard.session.as_mut() {
                            Some(existing) => match decide_existing(
                                &existing.launcher.identity.username,
                                existing.connected,
                                &pending
                                    .as_ref()
                                    .expect("pending launcher")
                                    .identity
                                    .username,
                            ) {
                                Arbitration::Reconnect => {
                                    let requested_plan =
                                        multi_monitor.as_ref().map(|(plan, _carrier)| plan);
                                    let topology_matches =
                                        existing.multi_monitor_plan.as_ref() == requested_plan;
                                    // The current client layout is
                                    // authoritative. A disconnected desktop
                                    // with a different frozen topology is
                                    // replaced rather than silently reattached.
                                    if reconnect_must_replace_desktop(topology_matches) {
                                        guard.draining = true;
                                        ExistingDecision::Replace(Box::new(
                                            guard.session.take().expect("existing session"),
                                        ))
                                    } else {
                                        let requested_timezone =
                                            pending.as_ref().expect("pending launcher").timezone();
                                        if timezone_mismatch(
                                            existing.timezone.as_ref(),
                                            requested_timezone,
                                        ) {
                                            tracing::warn!(
                                                target: crate::logging::target::SESSION,
                                                active_timezone = existing
                                                    .timezone
                                                    .as_ref()
                                                    .map_or("<none>", IanaTimeZone::as_str),
                                                requested_timezone = requested_timezone
                                                    .map_or("<none>", IanaTimeZone::as_str),
                                                "reconnect timezone differs from persistent desktop; retaining active timezone"
                                            );
                                        }
                                        existing.connected = true;
                                        existing.disconnected_since = None;
                                        let replacement = pending
                                            .as_ref()
                                            .expect("pending launcher")
                                            .session_log_id();
                                        let previous = existing
                                            .launcher
                                            .replace_session_log_id(replacement.clone());
                                        ExistingDecision::Reconnect(Box::new((
                                            existing.metadata(true),
                                            existing.execution(),
                                            replacement,
                                            previous,
                                        )))
                                    }
                                }
                                Arbitration::Busy => ExistingDecision::Busy,
                            },
                            None => ExistingDecision::Create,
                        }
                    }
                }
            };

            match decision {
                ExistingDecision::Dead(session) => {
                    tracing::warn!(
                        target: crate::logging::target::SESSION,
                        username = %session.launcher.identity.username,
                        "session launcher died; tearing down stale desktop state"
                    );
                    let completion = self.spawn_cleanup(async move {
                        (*session).shutdown().await;
                    });
                    wait_for_cleanup_completion(completion).await;
                }
                ExistingDecision::Replace(session) => {
                    tracing::warn!(
                        target: crate::logging::target::SESSION,
                        username = %session.launcher.identity.username,
                        "client authorised replacing a persistent desktop whose committed topology cannot serve the requested layout; tearing it down for a fresh create"
                    );
                    let completion = self.spawn_cleanup(async move {
                        (*session).shutdown().await;
                    });
                    wait_for_cleanup_completion(completion).await;
                }
                ExistingDecision::IdleExpired(session, disconnected_for, configured_limit) => {
                    tracing::info!(
                        target: crate::logging::target::SESSION,
                        username = %session.launcher.identity.username,
                        disconnected_age_seconds = disconnected_for.as_secs(),
                        configured_limit_seconds = configured_limit.as_secs(),
                        "disconnected persistent desktop exceeded configured idle lifetime; tearing it down for a fresh create"
                    );
                    let completion = self.spawn_cleanup(async move {
                        (*session).shutdown().await;
                    });
                    wait_for_cleanup_completion(completion).await;
                }
                ExistingDecision::Draining => {
                    self.wait_for_cleanup().await;
                }
                ExistingDecision::Reconnect(reconnect) => {
                    let (metadata, execution, session_log_id, previous_session_log_id) = *reconnect;
                    pending.take().expect("pending launcher").discard().await;
                    tracing::info!(
                        target: crate::logging::target::SESSION,
                        sid = %session_log_id,
                        previous_sid = %previous_session_log_id,
                        username = %metadata.username,
                        uid = metadata.uid,
                        session_id = %metadata.session_id,
                        generation = metadata.generation,
                        "reattached to persistent authenticated desktop"
                    );
                    return Ok(SessionLease::new(Arc::downgrade(self), metadata, execution));
                }
                ExistingDecision::Busy => {
                    pending.take().expect("pending launcher").discard().await;
                    return Err(LifecycleError::Busy);
                }
                ExistingDecision::Create => break,
            }
        }

        let mut pending = pending.take().expect("pending launcher");
        let mut timezone = pending.timezone().cloned();
        let original_timezone = std::env::var("TZ").ok();
        let mut timezone_lease = prepare_timezone_for_open(
            &pending.identity,
            &pending.session_log_id(),
            &mut timezone,
            original_timezone.as_deref(),
        );
        pending.set_timezone(timezone.clone());
        let launcher = match pending.open().await {
            Ok(launcher) => launcher,
            Err(error) => {
                if complete_timezone_restore_after_shutdown(&mut timezone_lease, true).is_err() {
                    tracing::warn!(
                        target: crate::logging::target::SESSION,
                        "timezone lease transition failed after awaited launcher open cleanup"
                    );
                }
                return Err(error.into());
            }
        };
        if mark_timezone_applied(&mut timezone_lease).is_err() {
            let agent_exit_confirmed = launcher.shutdown().await;
            let _ =
                complete_timezone_restore_after_shutdown(&mut timezone_lease, agent_exit_confirmed);
            return Err(LifecycleError::TimezoneLease);
        }
        let desktop = DesktopSession {
            launcher,
            connected: true,
            disconnected_since: None,
            generation: 1,
            created_at: SystemTime::now(),
            timezone,
            timezone_lease,
            held_display: None,
            multi_monitor_plan: multi_monitor.as_ref().map(|(plan, _carrier)| plan.clone()),
            multi_monitor_carrier: multi_monitor.map(|(_plan, carrier)| carrier),
            multi_monitor_ever_usable: MultiMonitorEverUsable::new(),
        };
        let metadata = desktop.metadata(false);
        let execution = desktop.execution();
        {
            let mut guard = self.inner.lock().unwrap();
            debug_assert!(
                guard.session.is_none() && !guard.draining,
                "connection permit serializes session creation"
            );
            guard.session = Some(desktop);
        }
        tracing::info!(
            target: crate::logging::target::SESSION,
            username = %metadata.username,
            uid = metadata.uid,
            session_id = %metadata.session_id,
            session_type = %metadata.session_type,
            desktop = %metadata.desktop,
            display = %metadata.display,
            agent_pid = metadata.agent_pid,
            "persistent authenticated graphical session created"
        );
        Ok(SessionLease::new(Arc::downgrade(self), metadata, execution))
    }

    pub fn request_shutdown(&self) {
        if let Err(error) = self.resume.shutdown() {
            tracing::error!(
                target: crate::logging::target::SESSION,
                ?error,
                "resume authority shutdown request failed closed"
            );
        }
    }

    pub async fn shutdown(self: &Arc<Self>) {
        if let Err(error) = self.resume.shutdown() {
            tracing::error!(
                target: crate::logging::target::SESSION,
                ?error,
                "resume authority shutdown failed closed"
            );
        }
        let session = {
            let mut guard = self.inner.lock().unwrap();
            if guard.draining {
                None
            } else {
                let session = guard.session.take();
                guard.draining = session.is_some();
                session
            }
        };
        if let Some(session) = session {
            self.spawn_cleanup(async move {
                session.shutdown().await;
            });
        }
        self.join_cleanup().await;
        if let Err(error) = self.resume.complete_shutdown() {
            tracing::error!(
                target: crate::logging::target::SESSION,
                ?error,
                "resume authority shutdown completion failed closed"
            );
        }
    }

    pub fn hold_display(
        &self,
        generation: u64,
        username: &str,
        resources: HeldDisplayResources,
    ) -> Result<(), LifecycleError> {
        let mut guard = self.inner.lock().unwrap();
        let session = guard
            .session
            .as_mut()
            .ok_or(LifecycleError::DisplayOwnership)?;
        if session.generation != generation
            || session.launcher.identity.username != username
            || session.held_display.is_some()
        {
            return Err(LifecycleError::DisplayOwnership);
        }
        session.held_display = Some(resources);
        Ok(())
    }

    pub fn take_held_display(
        &self,
        generation: u64,
        username: &str,
    ) -> Result<HeldDisplayResources, LifecycleError> {
        let mut guard = self.inner.lock().unwrap();
        let session = guard
            .session
            .as_mut()
            .ok_or(LifecycleError::DisplayOwnership)?;
        if session.generation != generation || session.launcher.identity.username != username {
            return Err(LifecycleError::DisplayOwnership);
        }
        session
            .held_display
            .take()
            .ok_or(LifecycleError::DisplayOwnership)
    }

    /// Atomically latches that the persistent desktop identified by
    /// `(generation, username)` has reached a usable multi-monitor
    /// attachment at least once. A no-op if the desktop has since been
    /// replaced/removed or `generation`/`username` no longer match the
    /// live desktop (a stale attachment can never mutate a different
    /// generation's state), and a no-op (not a regression) if already
    /// latched `true`. See [`MultiMonitorEverUsable`].
    pub fn mark_multi_monitor_ever_usable(&self, generation: u64, username: &str) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(session) = guard.session.as_mut() {
            if session.generation == generation && session.launcher.identity.username == username {
                session.multi_monitor_ever_usable.mark();
            }
        }
    }

    /// Whether the persistent desktop identified by `(generation,
    /// username)` has ever reached a usable multi-monitor attachment,
    /// across any past attachment (including reconnects) since its
    /// currently committed plan was last (re)committed. Returns `false`
    /// if the desktop no longer exists or `generation`/`username` no
    /// longer match — the same safe default a freshly created desktop
    /// starts at. See [`MultiMonitorEverUsable`].
    pub fn multi_monitor_ever_usable(&self, generation: u64, username: &str) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.session.as_ref().is_some_and(|session| {
            session.generation == generation
                && session.launcher.identity.username == username
                && session.multi_monitor_ever_usable.get()
        })
    }

    pub async fn validate_native_session(
        &self,
        metadata: &SessionMetadata,
    ) -> Result<(), LifecycleError> {
        let (session_id, uid) = {
            let mut guard = self.inner.lock().unwrap();
            let session = guard
                .session
                .as_mut()
                .ok_or(LifecycleError::NativeSessionChanged)?;
            if session.generation != metadata.generation
                || session.launcher.identity.username != metadata.username
                || session.launcher.identity.uid != metadata.uid
                || !native_processes_healthy(
                    session.launcher.is_running(),
                    session.launcher.agent_is_running(),
                )
            {
                return Err(LifecycleError::NativeSessionChanged);
            }
            (
                session
                    .launcher
                    .environment
                    .session_id()
                    .unwrap_or_default()
                    .to_string(),
                session.launcher.identity.uid,
            )
        };
        validate_active_logind_session(&session_id, uid)
            .await
            .map_err(|_| LifecycleError::NativeSessionChanged)
    }

    pub fn replace_session_log_id(
        &self,
        metadata: &SessionMetadata,
        replacement: CorrelationId,
    ) -> Result<CorrelationId, LifecycleError> {
        let guard = self.inner.lock().unwrap();
        let session = guard
            .session
            .as_ref()
            .ok_or(LifecycleError::NativeSessionChanged)?;
        if session.generation != metadata.generation
            || session.launcher.identity.username != metadata.username
        {
            return Err(LifecycleError::NativeSessionChanged);
        }
        Ok(session.launcher.replace_session_log_id(replacement))
    }

    fn start_termination(
        self: &Arc<Self>,
        generation: u64,
        username: &str,
    ) -> Option<watch::Receiver<bool>> {
        let session = {
            let mut guard = self.inner.lock().unwrap();
            if !guard.draining
                && guard.session.as_ref().is_some_and(|session| {
                    session.generation == generation
                        && session.launcher.identity.username == username
                })
            {
                guard.draining = true;
                guard.session.take()
            } else {
                None
            }
        };
        if let Some(session) = session {
            Some(self.spawn_cleanup(async move {
                session.shutdown().await;
            }))
        } else {
            self.cleanup
                .lock()
                .unwrap()
                .as_ref()
                .map(|task| task.completed.clone())
        }
    }

    fn disconnect(&self, generation: u64, username: &str) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(session) = guard.session.as_mut() {
            if session.generation == generation && session.launcher.identity.username == username {
                if session.connected {
                    session.connected = false;
                    session.disconnected_since = Some(SystemTime::now());
                }
                tracing::info!(
                    target: crate::logging::target::SESSION,
                    username,
                    generation,
                    "client detached; graphical session persists for reconnect"
                );
            }
        }
    }

    fn spawn_cleanup<F>(self: &Arc<Self>, cleanup: F) -> watch::Receiver<bool>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let (completed_tx, completed_rx) = watch::channel(false);
        let registry = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            let _completion = CleanupCompletion {
                registry,
                completed: Some(completed_tx),
            };
            cleanup.await;
        });
        let mut slot = self.cleanup.lock().unwrap();
        debug_assert!(
            slot.as_ref().is_none_or(|task| task.handle.is_finished()),
            "only one desktop cleanup may run"
        );
        *slot = Some(CleanupTask {
            handle,
            completed: completed_rx.clone(),
        });
        completed_rx
    }

    async fn wait_for_cleanup(&self) {
        loop {
            if !self.inner.lock().unwrap().draining {
                return;
            }
            let completion = self
                .cleanup
                .lock()
                .unwrap()
                .as_ref()
                .map(|task| task.completed.clone());
            if let Some(completion) = completion {
                wait_for_cleanup_completion(completion).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
    }

    async fn join_cleanup(&self) {
        self.wait_for_cleanup().await;
        let task = self.cleanup.lock().unwrap().take();
        if let Some(task) = task {
            if let Err(error) = task.handle.await {
                tracing::error!(
                    target: crate::logging::target::SESSION,
                    %error,
                    "registry-owned desktop cleanup task failed"
                );
            }
        }
    }
}

pub struct HeldDisplayResources {
    guard: Option<MetaModeGuard>,
    permit: Option<OwnedSemaphorePermit>,
}

impl HeldDisplayResources {
    pub fn new(guard: Option<MetaModeGuard>, permit: Option<OwnedSemaphorePermit>) -> Self {
        Self { guard, permit }
    }

    pub async fn restore(&mut self) {
        if let Some(guard) = self.guard.as_mut() {
            if let Err(error) = guard.restore().await {
                tracing::warn!(
                    target: crate::logging::target::DISPLAY,
                    %error,
                    "failed to restore held NV-CONTROL MetaMode"
                );
            }
        }
        self.guard.take();
        self.permit.take();
    }

    /// Physical raster (X11 virtual screen) size from the pre-session MetaMode,
    /// if a display guard is held.  Used to size the uinput ABS range correctly.
    pub fn raster_size(&self) -> Option<(u32, u32)> {
        self.guard.as_ref()?.raster_size()
    }

    /// True when this session actually holds a display guard; the attachment
    /// also gates live resize on the negotiated display mode.
    pub fn can_reassign(&self) -> bool {
        self.guard.is_some()
    }

    pub fn resolution(&self) -> Option<crate::display::nvctrl::Resolution> {
        self.guard.as_ref().map(MetaModeGuard::resolution)
    }

    /// Re-target the held display to a new resolution mid-session. The
    /// original pre-session restore target is preserved by the guard.
    pub async fn reassign(
        &mut self,
        resolution: crate::display::nvctrl::Resolution,
    ) -> Result<(), crate::display::nvctrl::DisplayError> {
        match self.guard.as_mut() {
            Some(guard) => guard.reassign(resolution).await,
            None => Err(crate::display::nvctrl::DisplayError::Command(
                "no display guard held for this session".to_string(),
            )),
        }
    }
}

enum ExistingDecision {
    Dead(Box<DesktopSession>),
    /// The existing desktop is healthy but cannot serve this connection's
    /// current topology. Torn down exactly like [`Self::Dead`], then the loop
    /// re-runs and takes the [`Self::Create`] arm.
    Replace(Box<DesktopSession>),
    /// The existing desktop is healthy but has remained disconnected longer
    /// than the configured idle lifetime. Torn down through the same cleanup
    /// path as [`Self::Dead`] and [`Self::Replace`], then retried as Create.
    IdleExpired(Box<DesktopSession>, Duration, Duration),
    Reconnect(Box<(SessionMetadata, UserExecution, CorrelationId, CorrelationId)>),
    Draining,
    Busy,
    Create,
}

async fn wait_for_cleanup_completion(mut completed: watch::Receiver<bool>) {
    while !*completed.borrow() {
        if completed.changed().await.is_err() {
            break;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arbitration {
    Reconnect,
    Busy,
}

fn native_processes_healthy(launcher_alive: bool, agent_alive: bool) -> bool {
    launcher_alive && agent_alive
}

fn decide_existing(owner: &str, connected: bool, requested: &str) -> Arbitration {
    if owner == requested && !connected {
        Arbitration::Reconnect
    } else {
        Arbitration::Busy
    }
}

fn timezone_mismatch(active: Option<&IanaTimeZone>, requested: Option<&IanaTimeZone>) -> bool {
    active != requested
}

fn prepare_timezone_lease(
    identity: &super::identity::UserIdentity,
    session_log_id: &CorrelationId,
    timezone: Option<&IanaTimeZone>,
    original_timezone: Option<&str>,
) -> Result<Option<RestoreLease>, LifecycleError> {
    let Some(timezone) = timezone else {
        return Ok(None);
    };
    let owner = LeaseOwnerId::new(format!(
        "linux-desktop:{}:{}:{}",
        identity.username, identity.uid, session_log_id
    ))
    .map_err(|_| LifecycleError::TimezoneLease)?;
    let original =
        original_timezone.map_or_else(|| "TZ=<absent>".to_string(), |value| format!("TZ={value}"));
    let original = StateFingerprint::from_bytes(original.as_bytes())
        .map_err(|_| LifecycleError::TimezoneLease)?;
    let target = format!("TZ={}", timezone.as_str());
    let target = StateFingerprint::from_bytes(target.as_bytes())
        .map_err(|_| LifecycleError::TimezoneLease)?;
    let mut lease = RestoreLease::arm(RestoreResource::Timezone, owner, original, target);
    lease
        .apply(RestoreEvent::BeginApply)
        .map_err(|_| LifecycleError::TimezoneLease)?;
    Ok(Some(lease))
}

fn prepare_timezone_for_open(
    identity: &super::identity::UserIdentity,
    session_log_id: &CorrelationId,
    timezone: &mut Option<IanaTimeZone>,
    original_timezone: Option<&str>,
) -> Option<RestoreLease> {
    match prepare_timezone_lease(
        identity,
        session_log_id,
        timezone.as_ref(),
        original_timezone,
    ) {
        Ok(lease) => lease,
        Err(error) => {
            tracing::warn!(
                target: crate::logging::target::SESSION,
                sid = %session_log_id,
                %error,
                "timezone bookkeeping unavailable; opening authenticated desktop without timezone redirection"
            );
            *timezone = None;
            None
        }
    }
}

fn mark_timezone_applied(lease: &mut Option<RestoreLease>) -> Result<(), LifecycleError> {
    if let Some(lease) = lease {
        lease
            .apply(RestoreEvent::ApplySucceeded)
            .map_err(|_| LifecycleError::TimezoneLease)?;
    }
    Ok(())
}

fn complete_timezone_restore(lease: &mut Option<RestoreLease>) -> Result<(), LifecycleError> {
    if let Some(lease) = lease {
        lease
            .apply(RestoreEvent::BeginRestore)
            .and_then(|_| lease.apply(RestoreEvent::RestoreSucceeded))
            .map_err(|_| LifecycleError::TimezoneLease)?;
    }
    Ok(())
}

fn complete_timezone_restore_after_shutdown(
    lease: &mut Option<RestoreLease>,
    agent_exit_confirmed: bool,
) -> Result<(), LifecycleError> {
    if lease.is_some() && !agent_exit_confirmed {
        return Err(LifecycleError::TimezoneLease);
    }
    complete_timezone_restore(lease)
}

pub struct SessionLease {
    registry: Weak<SessionRegistry>,
    pub metadata: SessionMetadata,
    pub execution: UserExecution,
    released: bool,
}

pub struct SessionTermination {
    completion: Option<watch::Receiver<bool>>,
}

impl SessionTermination {
    pub async fn wait(self) {
        if let Some(completion) = self.completion {
            wait_for_cleanup_completion(completion).await;
        }
    }
}

impl SessionLease {
    fn new(
        registry: Weak<SessionRegistry>,
        metadata: SessionMetadata,
        execution: UserExecution,
    ) -> Self {
        Self {
            registry,
            metadata,
            execution,
            released: false,
        }
    }

    pub fn disconnect(mut self) {
        self.release();
    }

    pub async fn terminate(self) {
        self.start_terminate().wait().await;
    }

    pub fn start_terminate(mut self) -> SessionTermination {
        let completion = self.registry.upgrade().and_then(|registry| {
            registry.start_termination(self.metadata.generation, &self.metadata.username)
        });
        self.released = true;
        SessionTermination { completion }
    }

    pub fn timezone(&self) -> Option<&IanaTimeZone> {
        self.metadata.timezone.as_ref()
    }

    /// See [`SessionRegistry::mark_multi_monitor_ever_usable`].
    pub fn mark_multi_monitor_ever_usable(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .mark_multi_monitor_ever_usable(self.metadata.generation, &self.metadata.username);
        }
    }

    /// See [`SessionRegistry::multi_monitor_ever_usable`].
    pub fn multi_monitor_ever_usable(&self) -> bool {
        self.registry.upgrade().is_some_and(|registry| {
            registry.multi_monitor_ever_usable(self.metadata.generation, &self.metadata.username)
        })
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        if let Some(registry) = self.registry.upgrade() {
            registry.disconnect(self.metadata.generation, &self.metadata.username);
        }
        self.released = true;
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A desktop's committed topology is frozen for its lifetime, so a
    /// desktop created without one can never grow into Match My Layout on a
    /// later reconnect. Replacing it loses the user's running applications,
    /// so it must happen only on an explicit client instruction -- never
    /// inferred from the mismatch itself.
    ///
    /// The 2026-08-11 pier-linux.example.internal case is the first row: a desktop created
    /// without a plan, still alive and disconnected 85 minutes later,
    /// refusing a two-monitor Match My Layout connect.
    #[test]
    fn every_topology_mismatch_replaces_the_disconnected_desktop() {
        assert!(reconnect_must_replace_desktop(false));

        // Exact topology matches always reattach, regardless of permission.
        assert!(!reconnect_must_replace_desktop(true));
    }

    /// A disconnected desktop's idle-lifetime policy is opt-in hygiene, not
    /// a new persistence default: absent configuration keeps the historical
    /// "persist forever for reconnect" behaviour exactly.
    ///
    /// When an operator does set a limit, only the disconnected age matters.
    /// A connected desktop is never eligible, and a desktop exactly at the
    /// configured boundary is retained until it is strictly older than the
    /// limit.
    #[test]
    fn only_over_limit_disconnected_desktops_exceed_idle_lifetime() {
        let limit = Duration::from_secs(60);

        assert!(!disconnected_idle_lifetime_expired(
            false,
            Some(Duration::from_secs(3_600)),
            None
        ));
        assert!(!disconnected_idle_lifetime_expired(
            true,
            Some(Duration::from_secs(3_600)),
            Some(limit)
        ));
        assert!(!disconnected_idle_lifetime_expired(
            false,
            Some(Duration::from_secs(59)),
            Some(limit)
        ));
        assert!(disconnected_idle_lifetime_expired(
            false,
            Some(Duration::from_secs(61)),
            Some(limit)
        ));
        assert!(!disconnected_idle_lifetime_expired(
            false,
            Some(limit),
            Some(limit)
        ));
    }

    #[test]
    fn disconnected_owner_can_reconnect() {
        assert_eq!(
            decide_existing("artist", false, "artist"),
            Arbitration::Reconnect
        );
    }

    #[test]
    fn connected_or_different_user_is_busy() {
        assert_eq!(decide_existing("artist", true, "artist"), Arbitration::Busy);
        assert_eq!(decide_existing("artist", false, "other"), Arbitration::Busy);
    }

    #[test]
    fn multi_monitor_ever_usable_defaults_false_for_a_fresh_desktop() {
        // A brand-new desktop/plan (constructed at `Create`) always starts
        // unproven — matching both "reset on new desktop/plan" and
        // "session removal resets" (a removed desktop's replacement is
        // always a fresh `Create`, which always constructs a fresh, unproven
        // instance).
        assert!(!MultiMonitorEverUsable::new().get());
        assert!(!MultiMonitorEverUsable::default().get());
    }

    #[test]
    fn multi_monitor_ever_usable_latches_and_never_regresses() {
        let mut flag = MultiMonitorEverUsable::new();
        assert!(!flag.get());
        flag.mark();
        assert!(flag.get());
        // A later mark (e.g. a subsequent attachment also reaching usable,
        // or a late-arriving concurrent mark) must never be observable as
        // a regression back to `false` — the latch is monotonic.
        flag.mark();
        assert!(flag.get());
    }

    #[test]
    fn dead_launcher_or_agent_is_terminal_for_resume() {
        assert!(native_processes_healthy(true, true));
        assert!(!native_processes_healthy(false, true));
        assert!(!native_processes_healthy(true, false));
        assert!(!native_processes_healthy(false, false));
    }

    fn artist() -> super::super::identity::UserIdentity {
        super::super::identity::UserIdentity {
            username: "artist".into(),
            uid: 1001,
            gid: 100,
            supplementary_groups: vec![100],
            home: "/home/artist".into(),
            shell: "/bin/bash".into(),
        }
    }

    #[test]
    fn reconnect_timezone_adjudication_retains_existing_decision() {
        let oslo = IanaTimeZone::parse("Europe/Oslo").unwrap();
        let london = IanaTimeZone::parse("Europe/London").unwrap();
        assert!(!timezone_mismatch(Some(&oslo), Some(&oslo)));
        assert!(timezone_mismatch(Some(&oslo), Some(&london)));
        assert!(timezone_mismatch(Some(&oslo), None));
        assert!(timezone_mismatch(None, Some(&oslo)));
    }

    #[test]
    fn timezone_lease_is_deterministic_and_applied_only_when_marked_ready() {
        let timezone = IanaTimeZone::parse("Europe/Oslo").unwrap();
        let sid = CorrelationId::from_uuid_v4_bytes([7; 16]);
        let mut first = prepare_timezone_lease(&artist(), &sid, Some(&timezone), None).unwrap();
        let second = prepare_timezone_lease(&artist(), &sid, Some(&timezone), None).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.as_ref().map(RestoreLease::phase),
            Some(RestorePhase::Applying)
        );
        mark_timezone_applied(&mut first).unwrap();
        assert_eq!(
            first.as_ref().map(RestoreLease::phase),
            Some(RestorePhase::Applied)
        );
    }

    #[test]
    fn overlong_owner_fails_open_and_disables_timezone_before_launcher_open() {
        let mut identity = artist();
        identity.username = "a".repeat(256);
        let timezone = IanaTimeZone::parse("Europe/Oslo").unwrap();
        let sid = CorrelationId::from_uuid_v4_bytes([8; 16]);
        let mut timezone = Some(timezone);

        let lease = prepare_timezone_for_open(&identity, &sid, &mut timezone, None);

        assert!(lease.is_none());
        assert!(timezone.is_none());
    }

    #[test]
    fn shutdown_and_dead_cleanup_complete_timezone_restore_lease() {
        let timezone = IanaTimeZone::parse("Europe/Oslo").unwrap();
        let sid = CorrelationId::from_uuid_v4_bytes([9; 16]);
        let mut lease =
            prepare_timezone_lease(&artist(), &sid, Some(&timezone), Some("UTC")).unwrap();
        mark_timezone_applied(&mut lease).unwrap();
        complete_timezone_restore_after_shutdown(&mut lease, true).unwrap();
        assert_eq!(
            lease.as_ref().map(RestoreLease::phase),
            Some(RestorePhase::Restored)
        );
    }

    #[test]
    fn timezone_lease_waits_for_agent_exit_evidence() {
        let timezone = IanaTimeZone::parse("Europe/Oslo").unwrap();
        let sid = CorrelationId::from_uuid_v4_bytes([10; 16]);
        let mut lease =
            prepare_timezone_lease(&artist(), &sid, Some(&timezone), Some("UTC")).unwrap();
        mark_timezone_applied(&mut lease).unwrap();

        assert!(complete_timezone_restore_after_shutdown(&mut lease, false).is_err());
        assert_eq!(
            lease.as_ref().map(RestoreLease::phase),
            Some(RestorePhase::Applied)
        );
        complete_timezone_restore_after_shutdown(&mut lease, true).unwrap();
        assert_eq!(
            lease.as_ref().map(RestoreLease::phase),
            Some(RestorePhase::Restored)
        );
    }

    #[tokio::test]
    async fn cancelled_terminator_cannot_orphan_registry_owned_cleanup() {
        let registry = SessionRegistry::new(None).unwrap();
        registry.inner.lock().unwrap().draining = true;
        let cleaned = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let terminator_registry = Arc::clone(&registry);
        let terminator_cleaned = Arc::clone(&cleaned);
        let terminator = tokio::spawn(async move {
            let completion = terminator_registry.spawn_cleanup(async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                terminator_cleaned.store(true, Ordering::Release);
            });
            wait_for_cleanup_completion(completion).await;
        });
        started_rx.await.unwrap();
        terminator.abort();
        let _ = terminator.await;

        let shutdown_registry = Arc::clone(&registry);
        let mut shutdown = tokio::spawn(async move {
            shutdown_registry.shutdown().await;
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut shutdown)
                .await
                .is_err(),
            "shutdown must join the still-running cleanup"
        );
        let _ = release_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(1), shutdown)
            .await
            .expect("shutdown joins cleanup")
            .unwrap();
        assert!(cleaned.load(Ordering::Acquire));
        assert!(registry.cleanup.lock().unwrap().is_none());
        assert!(!registry.inner.lock().unwrap().draining);
    }
}
