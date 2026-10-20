//! `org.gnome.SessionManager` on a headless session's private bus.
//!
//! A GDM login gets this service from `gnome-session`, which since GNOME 49
//! runs only under the user's systemd manager (outside the session's scope
//! and private bus). The agent serves the subset GNOME Shell and the
//! settings daemons use, as thin D-Bus glue around the shared model in
//! [`arcen_session::desktop_session::manager`]:
//!
//! - presence (`org.gnome.SessionManager.Presence`), driven by Mutter's
//!   `IdleMonitor` and `org.gnome.desktop.session idle-delay`, so the
//!   shell's screen shield and idle lock work as on a GDM login;
//! - inhibitors, which the shell's end-session dialog and the screensaver
//!   proxy (`org.freedesktop.ScreenSaver.Inhibit`) use, and which keep
//!   presence from going idle while a video plays;
//! - client registration (`DESKTOP_AUTOSTART_ID`), so the settings daemons
//!   register and are told to stop;
//! - `Logout`, through the shell's end-session dialog, which ends the
//!   desktop through the agent's orderly path;
//! - `Setenv`, which Mutter calls when Xwayland starts (`DISPLAY`).
//!
//! Where Arcen differs on purpose (`SessionIsActive=false`,
//! `CanShutdown=false`, no logind `IdleHint`) is
//! [`ARCEN_SESSION_MANAGER_POLICY`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arcen_session::desktop_session::manager::{
    timeouts, Clients, Inhibitors, LogoutMode, Presence, PresenceEvent, PresenceStatus,
    ARCEN_SESSION_MANAGER_POLICY, INHIBIT_IDLE, INHIBIT_LOGOUT,
};
use futures_util::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{interface, Connection, Proxy};

use super::stack::MemberEnvironment;
use crate::logging::target;

/// The well-known name.
pub const SESSION_MANAGER_NAME: &str = "org.gnome.SessionManager";
const MANAGER_PATH: &str = "/org/gnome/SessionManager";
const PRESENCE_PATH: &str = "/org/gnome/SessionManager/Presence";
const IDLE_MONITOR_NAME: &str = "org.gnome.Mutter.IdleMonitor";
const IDLE_MONITOR_PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";
const IDLE_MONITOR_INTERFACE: &str = "org.gnome.Mutter.IdleMonitor";
const SHELL_NAME: &str = "org.gnome.Shell";
const SCREENSAVER_NAME: &str = "org.gnome.ScreenSaver";
const SCREENSAVER_PATH: &str = "/org/gnome/ScreenSaver";
const END_SESSION_DIALOG_PATH: &str = "/org/gnome/SessionManager/EndSessionDialog";
const END_SESSION_DIALOG_INTERFACE: &str = "org.gnome.SessionManager.EndSessionDialog";
const GSETTINGS: &str = "/usr/bin/gsettings";
/// `org.gnome.desktop.session idle-delay`'s default.
const DEFAULT_IDLE_DELAY_SECS: u32 = 300;
const LONGEST_SETENV_VALUE: usize = 4096;
const IDLE_MONITOR_RETRY: Duration = Duration::from_millis(500);

/// Variables `Setenv` may set for later autostart programs: the X11
/// display Mutter announces, the keyring's own variables, and anything the
/// login-environment filter would also let in.
fn setenv_allowed(name: &str) -> bool {
    matches!(
        name,
        "DISPLAY" | "XAUTHORITY" | "WAYLAND_DISPLAY" | "GNOME_KEYRING_CONTROL" | "SSH_AUTH_SOCK"
    ) || arcen_session::desktop_session::login_environment::name_allowed(name)
}

#[derive(Debug, Default)]
struct State {
    inhibitors: Inhibitors,
    clients: Clients,
    presence: Option<Presence>,
    responses: BTreeMap<u32, bool>,
    logout_running: bool,
}

type Shared = Arc<Mutex<State>>;

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
enum Event {
    InhibitorsChanged,
    Logout(LogoutMode),
    SetStatus(PresenceStatus),
}

fn inhibitor_path(cookie: u32) -> String {
    format!("{MANAGER_PATH}/Inhibitor{cookie}")
}

fn client_path(id: u32) -> String {
    format!("{MANAGER_PATH}/Client{id}")
}

fn owned_path(path: &str) -> zbus::fdo::Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(path.to_owned())
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
}

fn caller(header: &Header<'_>) -> String {
    header
        .sender()
        .map_or_else(String::new, ToString::to_string)
}

struct ManagerIface {
    shared: Shared,
    events: mpsc::UnboundedSender<Event>,
    environment: watch::Sender<BTreeMap<String, String>>,
}

#[interface(name = "org.gnome.SessionManager")]
impl ManagerIface {
    async fn setenv(&self, variable: &str, value: &str) -> zbus::fdo::Result<()> {
        let plausible = !variable.is_empty()
            && variable.len() <= 128
            && variable
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && value.len() <= LONGEST_SETENV_VALUE
            && !value.contains('\0');
        if !plausible || !setenv_allowed(variable) {
            tracing::info!(target: target::SESSION, variable, "session manager Setenv refused");
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "{variable} is not settable in this session"
            )));
        }
        tracing::info!(target: target::SESSION, variable, "session manager Setenv");
        self.environment.send_modify(|environment| {
            environment.insert(variable.to_owned(), value.to_owned());
        });
        Ok(())
    }

    fn get_locale(&self, category: i32) -> String {
        let names: &[&str] = match category {
            0 => &["LC_CTYPE"],
            1 => &["LC_NUMERIC"],
            2 => &["LC_TIME"],
            3 => &["LC_COLLATE"],
            4 => &["LC_MONETARY"],
            5 => &["LC_MESSAGES"],
            _ => &[],
        };
        names
            .iter()
            .chain(["LANG"].iter())
            .find_map(|name| std::env::var(name).ok())
            .unwrap_or_else(|| "C".to_owned())
    }

    fn initialization_error(&self, message: &str, fatal: bool) {
        let message: String = message.chars().take(256).collect();
        tracing::warn!(target: target::SESSION, message, fatal, "a session component reported an initialization error");
    }

    async fn register_client(
        &self,
        app_id: &str,
        client_startup_id: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let owner = caller(&header);
        let registered = lock(&self.shared)
            .clients
            .register(app_id, client_startup_id, &owner);
        let (id, new) =
            registered.map_err(|_| zbus::fdo::Error::LimitsExceeded("too many clients".into()))?;
        let path = client_path(id);
        if new {
            server
                .at(
                    path.as_str(),
                    ClientIface {
                        id,
                        shared: Arc::clone(&self.shared),
                    },
                )
                .await?;
            server
                .at(
                    path.as_str(),
                    ClientPrivateIface {
                        id,
                        shared: Arc::clone(&self.shared),
                    },
                )
                .await?;
            let object = owned_path(&path)?;
            Self::client_added(&emitter, &object).await?;
            let app_id: String = app_id.chars().take(64).collect();
            tracing::info!(target: target::SESSION, app_id, client = id, "session client registered");
        }
        owned_path(&path)
    }

    async fn unregister_client(
        &self,
        client_id: ObjectPath<'_>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let id = client_id
            .as_str()
            .strip_prefix(&format!("{MANAGER_PATH}/Client"))
            .and_then(|id| id.parse::<u32>().ok())
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("not a client".into()))?;
        lock(&self.shared)
            .clients
            .unregister(id, &caller(&header))
            .map_err(|_| zbus::fdo::Error::InvalidArgs("not your client".into()))?;
        remove_client_objects(server, id).await;
        Self::client_removed(&emitter, &client_id).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // D-Bus arguments plus zbus's injected context.
    async fn inhibit(
        &self,
        app_id: &str,
        toplevel_xid: u32,
        reason: &str,
        flags: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<u32> {
        let owner = caller(&header);
        let cookie = lock(&self.shared)
            .inhibitors
            .add(app_id, reason, flags, toplevel_xid, &owner)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(format!("{error:?}")))?;
        let path = inhibitor_path(cookie);
        server
            .at(
                path.as_str(),
                InhibitorIface {
                    cookie,
                    shared: Arc::clone(&self.shared),
                },
            )
            .await?;
        let object = owned_path(&path)?;
        Self::inhibitor_added(&emitter, &object).await?;
        self.inhibited_actions_changed(&emitter).await?;
        let _ = self.events.send(Event::InhibitorsChanged);
        let app_id: String = app_id.chars().take(64).collect();
        tracing::info!(target: target::SESSION, app_id, flags, cookie, "session inhibitor added");
        Ok(cookie)
    }

    async fn uninhibit(
        &self,
        inhibit_cookie: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        lock(&self.shared)
            .inhibitors
            .remove(inhibit_cookie, &caller(&header))
            .map_err(|_| zbus::fdo::Error::InvalidArgs("unknown cookie".into()))?;
        remove_inhibitor_object(server, inhibit_cookie).await;
        let object = owned_path(&inhibitor_path(inhibit_cookie))?;
        Self::inhibitor_removed(&emitter, &object).await?;
        self.inhibited_actions_changed(&emitter).await?;
        let _ = self.events.send(Event::InhibitorsChanged);
        tracing::info!(target: target::SESSION, cookie = inhibit_cookie, "session inhibitor removed");
        Ok(())
    }

    fn is_inhibited(&self, flags: u32) -> bool {
        lock(&self.shared).inhibitors.is_inhibited(flags)
    }

    fn get_clients(&self) -> zbus::fdo::Result<Vec<OwnedObjectPath>> {
        let ids = lock(&self.shared).clients.ids();
        ids.into_iter()
            .map(|id| owned_path(&client_path(id)))
            .collect()
    }

    fn get_inhibitors(&self) -> zbus::fdo::Result<Vec<OwnedObjectPath>> {
        let cookies = lock(&self.shared).inhibitors.cookies();
        cookies
            .into_iter()
            .map(|cookie| owned_path(&inhibitor_path(cookie)))
            .collect()
    }

    fn is_autostart_condition_handled(&self, _condition: &str) -> bool {
        false
    }

    fn is_session_running(&self) -> bool {
        true
    }

    fn logout(&self, mode: u32) -> zbus::fdo::Result<()> {
        let mode = LogoutMode::from_value(mode)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown logout mode".into()))?;
        tracing::info!(target: target::SESSION, ?mode, "logout requested inside the session");
        let _ = self.events.send(Event::Logout(mode));
        Ok(())
    }

    fn shutdown(&self) -> zbus::fdo::Result<()> {
        tracing::info!(target: target::SESSION, "power off requested inside a remote session; refused (host power belongs to the operator)");
        Err(zbus::fdo::Error::NotSupported(
            "a remote Arcen session cannot power off the host".into(),
        ))
    }

    fn reboot(&self) -> zbus::fdo::Result<()> {
        tracing::info!(target: target::SESSION, "restart requested inside a remote session; refused (host power belongs to the operator)");
        Err(zbus::fdo::Error::NotSupported(
            "a remote Arcen session cannot restart the host".into(),
        ))
    }

    fn can_shutdown(&self) -> bool {
        ARCEN_SESSION_MANAGER_POLICY.can_shutdown
    }

    fn can_reboot_to_firmware_setup(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn session_name(&self) -> String {
        "gnome".to_owned()
    }

    #[zbus(property)]
    fn session_is_active(&self) -> bool {
        ARCEN_SESSION_MANAGER_POLICY.session_is_active
    }

    #[zbus(property)]
    fn inhibited_actions(&self) -> u32 {
        lock(&self.shared).inhibitors.inhibited_actions()
    }

    #[zbus(property)]
    fn renderer(&self) -> String {
        String::new()
    }

    #[zbus(signal)]
    async fn client_added(emitter: &SignalEmitter<'_>, id: &ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn client_removed(emitter: &SignalEmitter<'_>, id: &ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn inhibitor_added(emitter: &SignalEmitter<'_>, id: &ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn inhibitor_removed(
        emitter: &SignalEmitter<'_>,
        id: &ObjectPath<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn session_running(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn session_over(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

struct PresenceIface {
    shared: Shared,
    events: mpsc::UnboundedSender<Event>,
}

#[interface(name = "org.gnome.SessionManager.Presence")]
impl PresenceIface {
    fn set_status(&self, status: u32) -> zbus::fdo::Result<()> {
        let status = PresenceStatus::settable(status)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("not a settable status".into()))?;
        let _ = self.events.send(Event::SetStatus(status));
        Ok(())
    }

    #[zbus(property, name = "status")]
    fn status(&self) -> u32 {
        lock(&self.shared)
            .presence
            .map_or(PresenceStatus::Available, |presence| presence.status())
            .value()
    }

    #[zbus(signal, name = "StatusChanged")]
    async fn status_changed_signal(emitter: &SignalEmitter<'_>, status: u32) -> zbus::Result<()>;
}

struct InhibitorIface {
    cookie: u32,
    shared: Shared,
}

#[interface(name = "org.gnome.SessionManager.Inhibitor")]
impl InhibitorIface {
    fn get_app_id(&self) -> String {
        lock(&self.shared)
            .inhibitors
            .get(self.cookie)
            .map(|inhibitor| inhibitor.app_id.clone())
            .unwrap_or_default()
    }

    fn get_reason(&self) -> String {
        lock(&self.shared)
            .inhibitors
            .get(self.cookie)
            .map(|inhibitor| inhibitor.reason.clone())
            .unwrap_or_default()
    }

    fn get_flags(&self) -> u32 {
        lock(&self.shared)
            .inhibitors
            .get(self.cookie)
            .map_or(0, |inhibitor| inhibitor.flags)
    }

    fn get_toplevel_xid(&self) -> u32 {
        lock(&self.shared)
            .inhibitors
            .get(self.cookie)
            .map_or(0, |inhibitor| inhibitor.toplevel_xid)
    }

    fn get_client_id(&self) -> zbus::fdo::Result<OwnedObjectPath> {
        owned_path("/")
    }
}

struct ClientIface {
    id: u32,
    shared: Shared,
}

#[interface(name = "org.gnome.SessionManager.Client")]
impl ClientIface {
    fn get_app_id(&self) -> String {
        lock(&self.shared)
            .clients
            .get(self.id)
            .map(|client| client.app_id.clone())
            .unwrap_or_default()
    }

    fn get_startup_id(&self) -> String {
        lock(&self.shared)
            .clients
            .get(self.id)
            .map(|client| client.startup_id.clone())
            .unwrap_or_default()
    }

    fn get_restart_style_hint(&self) -> u32 {
        0
    }

    fn get_unix_process_id(&self) -> u32 {
        0
    }

    fn get_status(&self) -> u32 {
        1
    }

    async fn stop(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<()> {
        let path = client_path(self.id);
        let private = server
            .interface::<_, ClientPrivateIface>(path.as_str())
            .await?;
        ClientPrivateIface::stop(private.signal_emitter()).await?;
        Ok(())
    }
}

struct ClientPrivateIface {
    id: u32,
    shared: Shared,
}

#[interface(name = "org.gnome.SessionManager.ClientPrivate")]
impl ClientPrivateIface {
    fn end_session_response(&self, is_ok: bool, reason: &str) {
        let reason: String = reason.chars().take(128).collect();
        tracing::info!(target: target::SESSION, client = self.id, is_ok, reason, "session client answered the end of the session");
        lock(&self.shared).responses.insert(self.id, is_ok);
    }

    #[zbus(signal)]
    async fn stop(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn query_end_session(emitter: &SignalEmitter<'_>, flags: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn end_session(emitter: &SignalEmitter<'_>, flags: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn cancel_end_session(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

async fn remove_client_objects(server: &zbus::ObjectServer, id: u32) {
    let path = client_path(id);
    let _ = server.remove::<ClientIface, _>(path.as_str()).await;
    let _ = server.remove::<ClientPrivateIface, _>(path.as_str()).await;
}

async fn remove_inhibitor_object(server: &zbus::ObjectServer, cookie: u32) {
    let _ = server
        .remove::<InhibitorIface, _>(inhibitor_path(cookie).as_str())
        .await;
}

/// The running session manager.
pub struct SessionManager {
    connection: Connection,
    shared: Shared,
    environment: watch::Receiver<BTreeMap<String, String>>,
    logout: watch::Receiver<bool>,
    control: JoinHandle<()>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionManager")
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    /// Serves the session manager on the private bus at `bus_address` and
    /// claims its name. Call before the compositor starts, so the shell's
    /// first queries find it.
    ///
    /// # Errors
    ///
    /// When the bus is unreachable or the name is taken.
    pub async fn start(bus_address: &str, members: MemberEnvironment) -> Result<Self, zbus::Error> {
        let connection = zbus::connection::Builder::address(bus_address)?
            .build()
            .await?;
        let shared: Shared = Arc::new(Mutex::new(State::default()));
        let (events, events_rx) = mpsc::unbounded_channel();
        let (environment_tx, environment) = watch::channel(BTreeMap::new());
        let (logout_tx, logout) = watch::channel(false);
        connection
            .object_server()
            .at(
                MANAGER_PATH,
                ManagerIface {
                    shared: Arc::clone(&shared),
                    events: events.clone(),
                    environment: environment_tx,
                },
            )
            .await?;
        connection
            .object_server()
            .at(
                PRESENCE_PATH,
                PresenceIface {
                    shared: Arc::clone(&shared),
                    events,
                },
            )
            .await?;
        connection
            .request_name_with_flags(
                SESSION_MANAGER_NAME,
                zbus::fdo::RequestNameFlags::DoNotQueue.into(),
            )
            .await?;
        let control = tokio::spawn(control(
            connection.clone(),
            Arc::clone(&shared),
            events_rx,
            logout_tx,
            members,
        ));
        tracing::info!(
            target: target::SESSION,
            session_is_active = ARCEN_SESSION_MANAGER_POLICY.session_is_active,
            can_shutdown = ARCEN_SESSION_MANAGER_POLICY.can_shutdown,
            logind_idle_hint = ARCEN_SESSION_MANAGER_POLICY.sync_logind_idle_hint,
            "session manager serving org.gnome.SessionManager on the private bus"
        );
        Ok(Self {
            connection,
            shared,
            environment,
            logout,
            control,
        })
    }

    /// Variables components announced through `Setenv` (Mutter's
    /// `DISPLAY` when Xwayland starts).
    #[must_use]
    pub fn environment(&self) -> watch::Receiver<BTreeMap<String, String>> {
        self.environment.clone()
    }

    /// Turns true once the user confirmed a logout inside the session.
    #[must_use]
    pub fn logout(&self) -> watch::Receiver<bool> {
        self.logout.clone()
    }

    /// Tells every registered client to stop (orderly teardown), bounded.
    pub async fn stop_clients(&self) {
        let ids = lock(&self.shared).clients.ids();
        for id in ids {
            let path = client_path(id);
            if let Ok(private) = self
                .connection
                .object_server()
                .interface::<_, ClientPrivateIface>(path.as_str())
                .await
            {
                let _ = tokio::time::timeout(
                    Duration::from_millis(500),
                    ClientPrivateIface::stop(private.signal_emitter()),
                )
                .await;
            }
        }
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        self.control.abort();
    }
}

/// The session manager's own work: presence (idle watches, settings),
/// callers that left the bus, and the logout flow.
async fn control(
    connection: Connection,
    shared: Shared,
    mut events: mpsc::UnboundedReceiver<Event>,
    logout: watch::Sender<bool>,
    members: MemberEnvironment,
) {
    let idle_delay = read_idle_delay(&members)
        .await
        .unwrap_or(DEFAULT_IDLE_DELAY_SECS);
    lock(&shared).presence = Some(Presence::new(idle_delay));
    tracing::info!(target: target::SESSION, idle_delay, "session presence starts available");
    let mut delay_monitor = IdleDelayMonitor::spawn(&members);
    let mut owners = match zbus::fdo::DBusProxy::new(&connection).await {
        Ok(proxy) => proxy.receive_name_owner_changed().await.ok().map(|stream| {
            Drained::spawn(stream, |signal| {
                Some(signal.args().map_or_else(
                    |_| (String::new(), false),
                    |args| (args.name().to_string(), args.new_owner().is_none()),
                ))
            })
        }),
        Err(_) => None,
    };
    let mut idle: Option<IdleMonitor> = None;
    let mut shield: Option<Drained<bool>> = None;
    let mut retry = tokio::time::interval(IDLE_MONITOR_RETRY);
    let mut logout_task: Option<JoinHandle<()>> = None;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    Event::InhibitorsChanged => {
                        let inhibited = lock(&shared).inhibitors.is_inhibited(INHIBIT_IDLE);
                        apply_presence(&connection, &shared, PresenceEvent::IdleInhibited(inhibited)).await;
                        rearm(&shared, idle.as_mut()).await;
                    }
                    Event::SetStatus(status) => {
                        apply_presence(&connection, &shared, PresenceEvent::SetStatus(status)).await;
                    }
                    Event::Logout(mode) => {
                        let running = logout_task.as_ref().is_some_and(|task| !task.is_finished());
                        if running {
                            tracing::info!(target: target::SESSION, "a logout is already in progress");
                        } else {
                            logout_task = Some(tokio::spawn(run_logout(
                                connection.clone(),
                                Arc::clone(&shared),
                                mode,
                                logout.clone(),
                            )));
                        }
                    }
                }
            }
            changed = next_drained(owners.as_mut()) => {
                match changed {
                    Some((name, gone)) if gone && name.starts_with(':') => {
                        forget_owner(&connection, &shared, &name).await;
                        let inhibited = lock(&shared).inhibitors.is_inhibited(INHIBIT_IDLE);
                        apply_presence(&connection, &shared, PresenceEvent::IdleInhibited(inhibited)).await;
                        rearm(&shared, idle.as_mut()).await;
                    }
                    Some((name, _)) if name == IDLE_MONITOR_NAME => idle = None,
                    Some((name, gone)) if name == SCREENSAVER_NAME => {
                        shield = None;
                        // A shell that left the bus shows no shield.
                        if gone {
                            apply_presence(&connection, &shared, PresenceEvent::ScreenSaverActive(false)).await;
                            rearm(&shared, idle.as_mut()).await;
                        }
                    }
                    Some(_) => {}
                    None => owners = None,
                }
            }
            seconds = delay_monitor.next() => {
                match seconds {
                    Some(seconds) => {
                        tracing::info!(target: target::SESSION, idle_delay = seconds, "session idle delay changed");
                        apply_presence(&connection, &shared, PresenceEvent::IdleDelay(seconds)).await;
                        rearm(&shared, idle.as_mut()).await;
                    }
                    None => delay_monitor = IdleDelayMonitor::none(),
                }
            }
            _ = retry.tick(), if idle.is_none() || shield.is_none() => {
                if idle.is_none() {
                    if let Some(mut monitor) = IdleMonitor::connect(&connection).await {
                        rearm(&shared, Some(&mut monitor)).await;
                        tracing::info!(target: target::SESSION, "session presence follows the compositor's idle monitor");
                        idle = Some(monitor);
                    }
                }
                if shield.is_none() {
                    if let Some((stream, active)) = connect_shield(&connection).await {
                        tracing::info!(target: target::SESSION, active, "session presence follows the shell's screen shield");
                        shield = Some(stream);
                        apply_presence(&connection, &shared, PresenceEvent::ScreenSaverActive(active)).await;
                        rearm(&shared, idle.as_mut()).await;
                    }
                }
            }
            changed = next_drained(shield.as_mut()) => {
                match changed {
                    Some(active) => {
                        tracing::info!(target: target::SESSION, active, "screen shield changed");
                        apply_presence(&connection, &shared, PresenceEvent::ScreenSaverActive(active)).await;
                        rearm(&shared, idle.as_mut()).await;
                    }
                    None => shield = None,
                }
            }
            fired = next_watch(idle.as_mut()) => {
                match fired {
                    Some(id) => {
                        let Some(monitor) = idle.as_mut() else { continue };
                        if monitor.idle_watch.is_some_and(|(watch, _)| watch == id) {
                            apply_presence(&connection, &shared, PresenceEvent::IdleWatchFired).await;
                        } else if monitor.active_watch == Some(id) {
                            monitor.active_watch = None;
                            apply_presence(&connection, &shared, PresenceEvent::UserActive).await;
                        } else {
                            // A watch already replaced; re-arming for it
                            // would race the next one forever.
                            continue;
                        }
                        rearm(&shared, Some(monitor)).await;
                    }
                    None => idle = None,
                }
            }
        }
    }
}

/// A D-Bus signal stream drained by its own task into an unbounded channel.
///
/// zbus stops reading the whole connection while any match stream's queue
/// is full, and the control loop awaits D-Bus replies (Mutter's
/// `AddIdleWatch`, the shell's `GetActive`). Signals that arrive in a burst
/// during such an await (a browser starting puts dozens of
/// `NameOwnerChanged` on the bus) could fill a queue nobody was reading; the
/// awaited reply was then never read and the session manager stopped
/// answering the shell. A draining task keeps every queue moving.
struct Drained<T> {
    items: mpsc::UnboundedReceiver<T>,
    task: JoinHandle<()>,
}

impl<T: Send + 'static> Drained<T> {
    fn spawn<S, F>(stream: S, mut map: F) -> Self
    where
        S: futures_util::Stream + Send + 'static,
        F: FnMut(S::Item) -> Option<T> + Send + 'static,
    {
        let (sender, items) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            while let Some(item) = stream.next().await {
                if let Some(value) = map(item) {
                    if sender.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        Self { items, task }
    }
}

impl<T> Drop for Drained<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The next drained item; never resolves without a stream (a select arm
/// that resolved at once would spin the agent's single-threaded runtime).
async fn next_drained<T>(drained: Option<&mut Drained<T>>) -> Option<T> {
    let Some(drained) = drained else {
        return std::future::pending().await;
    };
    drained.items.recv().await
}

async fn forget_owner(connection: &Connection, shared: &Shared, owner: &str) {
    let (cookies, clients) = {
        let mut state = lock(shared);
        (
            state.inhibitors.remove_owner(owner),
            state.clients.remove_owner(owner),
        )
    };
    if cookies.is_empty() && clients.is_empty() {
        return;
    }
    let server = connection.object_server();
    let manager = server.interface::<_, ManagerIface>(MANAGER_PATH).await.ok();
    for cookie in &cookies {
        remove_inhibitor_object(server, *cookie).await;
        if let (Some(manager), Ok(path)) = (&manager, owned_path(&inhibitor_path(*cookie))) {
            let _ = ManagerIface::inhibitor_removed(manager.signal_emitter(), &path).await;
        }
    }
    for id in &clients {
        remove_client_objects(server, *id).await;
        if let (Some(manager), Ok(path)) = (&manager, owned_path(&client_path(*id))) {
            let _ = ManagerIface::client_removed(manager.signal_emitter(), &path).await;
        }
    }
    if let Some(manager) = &manager {
        let _ = manager
            .get()
            .await
            .inhibited_actions_changed(manager.signal_emitter())
            .await;
    }
    tracing::info!(
        target: target::SESSION,
        inhibitors = cookies.len(),
        clients = clients.len(),
        "a session bus peer left; its inhibitors and clients went with it"
    );
}

async fn apply_presence(connection: &Connection, shared: &Shared, event: PresenceEvent) {
    let changed = lock(shared)
        .presence
        .as_mut()
        .and_then(|presence| presence.apply(event));
    let Some(status) = changed else { return };
    tracing::info!(target: target::SESSION, status = ?status, "session presence changed");
    if let Ok(presence) = connection
        .object_server()
        .interface::<_, PresenceIface>(PRESENCE_PATH)
        .await
    {
        let emitter = presence.signal_emitter();
        let _ = PresenceIface::status_changed_signal(emitter, status.value()).await;
        let _ = presence.get().await.status_changed(emitter).await;
    }
}

struct IdleMonitor {
    proxy: Proxy<'static>,
    fired: Drained<u32>,
    /// The armed idle watch and its delay in seconds.
    idle_watch: Option<(u32, u32)>,
    active_watch: Option<u32>,
}

impl IdleMonitor {
    async fn connect(connection: &Connection) -> Option<Self> {
        let bus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
        let name = zbus::names::BusName::try_from(IDLE_MONITOR_NAME).ok()?;
        if !bus.name_has_owner(name).await.unwrap_or(false) {
            return None;
        }
        let proxy = Proxy::new(
            connection,
            IDLE_MONITOR_NAME,
            IDLE_MONITOR_PATH,
            IDLE_MONITOR_INTERFACE,
        )
        .await
        .ok()?;
        let fired = Drained::spawn(proxy.receive_signal("WatchFired").await.ok()?, |message| {
            Some(message.body().deserialize::<u32>().unwrap_or(0))
        });
        Some(Self {
            proxy,
            fired,
            idle_watch: None,
            active_watch: None,
        })
    }
}

/// The next fired watch; never resolves without a monitor.
async fn next_watch(monitor: Option<&mut IdleMonitor>) -> Option<u32> {
    let Some(monitor) = monitor else {
        return std::future::pending().await;
    };
    monitor.fired.items.recv().await
}

/// The shell's `org.gnome.ScreenSaver.ActiveChanged` signals and its
/// current state, once the shell owns the name.
async fn connect_shield(connection: &Connection) -> Option<(Drained<bool>, bool)> {
    let bus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
    let name = zbus::names::BusName::try_from(SCREENSAVER_NAME).ok()?;
    if !bus.name_has_owner(name).await.unwrap_or(false) {
        return None;
    }
    let proxy = Proxy::new(
        connection,
        SCREENSAVER_NAME,
        SCREENSAVER_PATH,
        SCREENSAVER_NAME,
    )
    .await
    .ok()?;
    let stream = Drained::spawn(
        proxy.receive_signal("ActiveChanged").await.ok()?,
        |message| message.body().deserialize::<bool>().ok(),
    );
    let active = proxy.call::<_, _, bool>("GetActive", &()).await.ok()?;
    Some((stream, active))
}

/// Keeps the compositor's watches matching presence: one persistent idle
/// watch while the session may go idle (replaced only when its delay
/// changes, since Mutter counts a new watch from the last input and fires
/// it at once when that is longer ago), and a one-shot user-active watch
/// while presence is idle or waits for activity after the shield went down.
async fn rearm(shared: &Shared, monitor: Option<&mut IdleMonitor>) {
    let Some(monitor) = monitor else { return };
    let (wants_activity, idle_secs) = {
        let state = lock(shared);
        let Some(presence) = state.presence else {
            return;
        };
        (presence.wants_activity_watch(), presence.idle_watch_secs())
    };
    if monitor.idle_watch.map(|(_, seconds)| seconds) != idle_secs {
        if let Some((id, _)) = monitor.idle_watch.take() {
            let _ = monitor.proxy.call_method("RemoveWatch", &(id,)).await;
        }
        if let Some(seconds) = idle_secs {
            monitor.idle_watch = monitor
                .proxy
                .call::<_, _, u32>("AddIdleWatch", &(u64::from(seconds) * 1000,))
                .await
                .ok()
                .map(|id| (id, seconds));
        }
    }
    if wants_activity && monitor.active_watch.is_none() {
        monitor.active_watch = monitor
            .proxy
            .call::<_, _, u32>("AddUserActiveWatch", &())
            .await
            .ok();
    }
}

/// `gsettings get org.gnome.desktop.session idle-delay`.
async fn read_idle_delay(members: &MemberEnvironment) -> Option<u32> {
    let mut command = members.command(GSETTINGS);
    command
        .args(["get", "org.gnome.desktop.session", "idle-delay"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(3), command.output())
        .await
        .ok()?
        .ok()?;
    parse_uint32(&String::from_utf8_lossy(&output.stdout))
}

/// The value of `uint32 300` (`gsettings get`) or
/// `idle-delay: uint32 300` (`gsettings monitor`).
fn parse_uint32(text: &str) -> Option<u32> {
    text.split_whitespace().last()?.parse().ok()
}

/// A bounded `gsettings monitor org.gnome.desktop.session idle-delay`.
struct IdleDelayMonitor {
    _child: Option<tokio::process::Child>,
    lines: Option<tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>>,
}

impl IdleDelayMonitor {
    const fn none() -> Self {
        Self {
            _child: None,
            lines: None,
        }
    }

    fn spawn(members: &MemberEnvironment) -> Self {
        let mut command = members.command(GSETTINGS);
        command
            .args(["monitor", "org.gnome.desktop.session", "idle-delay"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return Self::none();
        };
        let lines = child
            .stdout
            .take()
            .map(|stdout| tokio::io::BufReader::new(stdout).lines());
        Self {
            _child: Some(child),
            lines,
        }
    }

    async fn next(&mut self) -> Option<u32> {
        let Some(lines) = self.lines.as_mut() else {
            return std::future::pending().await;
        };
        loop {
            let line = lines.next_line().await.ok()??;
            if line.len() > 256 {
                continue;
            }
            if let Some(seconds) = parse_uint32(&line) {
                return Some(seconds);
            }
        }
    }
}

/// The logout flow: the shell's dialog when the mode asks for it, then
/// `EndSession` to every client (bounded), then the agent ends the desktop.
async fn run_logout(
    connection: Connection,
    shared: Shared,
    mode: LogoutMode,
    logout: watch::Sender<bool>,
) {
    let inhibitors = {
        let mut state = lock(&shared);
        state.logout_running = true;
        state.inhibitors.cookies_with(INHIBIT_LOGOUT)
    };
    let confirmed = if mode.shows_dialog(!inhibitors.is_empty()) {
        end_session_dialog(&connection, &inhibitors).await
    } else {
        true
    };
    if !confirmed {
        lock(&shared).logout_running = false;
        tracing::info!(target: target::SESSION, "logout cancelled");
        return;
    }
    let clients = {
        let mut state = lock(&shared);
        state.responses.clear();
        state.clients.ids()
    };
    for id in &clients {
        if let Ok(private) = connection
            .object_server()
            .interface::<_, ClientPrivateIface>(client_path(*id).as_str())
            .await
        {
            let _ = ClientPrivateIface::end_session(private.signal_emitter(), 0).await;
        }
    }
    let deadline = tokio::time::Instant::now() + timeouts::CLIENT_RESPONSE;
    while tokio::time::Instant::now() < deadline {
        let answered = {
            let state = lock(&shared);
            clients
                .iter()
                .all(|id| state.responses.contains_key(id) || state.clients.get(*id).is_none())
        };
        if answered {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if let Ok(manager) = connection
        .object_server()
        .interface::<_, ManagerIface>(MANAGER_PATH)
        .await
    {
        let _ = ManagerIface::session_over(manager.signal_emitter()).await;
    }
    tracing::info!(target: target::SESSION, clients = clients.len(), "user_logout: the user logged out inside the session; ending the desktop");
    let _ = logout.send(true);
}

/// Opens the shell's end-session dialog and waits for the answer. A shell
/// without the dialog (bare Mutter) logs out directly: the user asked for
/// it.
async fn end_session_dialog(connection: &Connection, inhibitors: &[u32]) -> bool {
    let Ok(proxy) = Proxy::new(
        connection,
        SHELL_NAME,
        END_SESSION_DIALOG_PATH,
        END_SESSION_DIALOG_INTERFACE,
    )
    .await
    else {
        return true;
    };
    let (Ok(mut confirmed), Ok(mut cancelled), Ok(mut closed)) = (
        proxy.receive_signal("ConfirmedLogout").await,
        proxy.receive_signal("Canceled").await,
        proxy.receive_signal("Closed").await,
    ) else {
        return true;
    };
    let paths = inhibitors
        .iter()
        .filter_map(|cookie| OwnedObjectPath::try_from(inhibitor_path(*cookie)).ok())
        .collect::<Vec<_>>();
    // Type 0 is logout.
    if let Err(error) = proxy
        .call_method("Open", &(0_u32, 0_u32, timeouts::DIALOG_SECONDS, paths))
        .await
    {
        tracing::info!(target: target::SESSION, %error, "the shell's end-session dialog is unavailable; logging out without it");
        return true;
    }
    // `biased`: a confirmation that arrives with another signal wins.
    tokio::select! {
        biased;
        _ = confirmed.next() => true,
        _ = cancelled.next() => false,
        _ = closed.next() => false,
        () = tokio::time::sleep(timeouts::DIALOG) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private bus for one test, in the build's target directory.
    struct TestBus {
        daemon: std::process::Child,
        dir: std::path::PathBuf,
    }

    impl TestBus {
        fn start(name: &str) -> Option<Self> {
            if !std::path::Path::new("/usr/bin/dbus-daemon").is_file() {
                return None;
            }
            let base = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
                || std::path::PathBuf::from("target"),
                std::path::PathBuf::from,
            );
            let dir = base.join(format!("sm-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).ok()?;
            let daemon = std::process::Command::new("/usr/bin/dbus-daemon")
                .arg("--session")
                .arg(format!("--address=unix:path={}", dir.join("bus").display()))
                .args(["--nofork", "--nopidfile"])
                .spawn()
                .ok()?;
            for _ in 0..100 {
                if dir.join("bus").exists() {
                    return Some(Self { daemon, dir });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            None
        }

        fn address(&self) -> String {
            format!("unix:path={}", self.dir.join("bus").display())
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn manager_proxy<'a>(
        connection: &'a Connection,
        path: &'a str,
        interface: &'a str,
    ) -> Proxy<'a> {
        Proxy::new(connection, SESSION_MANAGER_NAME, path, interface)
            .await
            .expect("proxy")
    }

    #[tokio::test]
    async fn the_session_manager_serves_inhibitors_clients_policy_and_logout() {
        let Some(bus) = TestBus::start("serve") else {
            eprintln!("no dbus-daemon on this host; skipped");
            return;
        };
        let members = MemberEnvironment::new(&bus.dir);
        let manager = SessionManager::start(&bus.address(), members)
            .await
            .expect("session manager");
        let mut environment = manager.environment();
        let mut logout = manager.logout();

        let client = zbus::connection::Builder::address(bus.address().as_str())
            .expect("address")
            .build()
            .await
            .expect("client");
        let sm = manager_proxy(&client, MANAGER_PATH, "org.gnome.SessionManager").await;

        assert!(!sm
            .get_property::<bool>("SessionIsActive")
            .await
            .expect("active"));
        assert!(!sm
            .call::<_, _, bool>("CanShutdown", &())
            .await
            .expect("can shutdown"));
        assert!(sm.call_method("Shutdown", &()).await.is_err());
        assert!(sm.call_method("Reboot", &()).await.is_err());

        let cookie: u32 = sm
            .call(
                "Inhibit",
                &("org.example.Player", 0_u32, "Playing", INHIBIT_IDLE),
            )
            .await
            .expect("inhibit");
        assert!(sm
            .call::<_, _, bool>("IsInhibited", &(INHIBIT_IDLE,))
            .await
            .expect("inhibited"));
        assert_eq!(
            sm.get_property::<u32>("InhibitedActions")
                .await
                .expect("actions"),
            INHIBIT_IDLE
        );
        let inhibitors: Vec<OwnedObjectPath> =
            sm.call("GetInhibitors", &()).await.expect("inhibitors");
        assert_eq!(inhibitors.len(), 1);
        let inhibitor = Proxy::new(
            &client,
            SESSION_MANAGER_NAME,
            inhibitors[0].as_str(),
            "org.gnome.SessionManager.Inhibitor",
        )
        .await
        .expect("inhibitor proxy");
        assert_eq!(
            inhibitor
                .call::<_, _, u32>("GetFlags", &())
                .await
                .expect("flags"),
            INHIBIT_IDLE
        );
        assert_eq!(
            inhibitor
                .call::<_, _, String>("GetAppId", &())
                .await
                .expect("app"),
            "org.example.Player"
        );

        let registered: OwnedObjectPath = sm
            .call("RegisterClient", &("gsd-power", "arcen-1-1"))
            .await
            .expect("register");
        assert!(registered
            .as_str()
            .starts_with("/org/gnome/SessionManager/Client"));
        let clients: Vec<OwnedObjectPath> = sm.call("GetClients", &()).await.expect("clients");
        assert_eq!(clients, vec![registered]);

        sm.call_method("Setenv", &("DISPLAY", ":7"))
            .await
            .expect("setenv");
        assert!(sm
            .call_method("Setenv", &("LD_PRELOAD", "/x.so"))
            .await
            .is_err());
        environment.changed().await.expect("environment");
        assert_eq!(
            environment.borrow().get("DISPLAY").map(String::as_str),
            Some(":7")
        );

        let presence =
            manager_proxy(&client, PRESENCE_PATH, "org.gnome.SessionManager.Presence").await;
        assert_eq!(
            presence
                .get_property::<u32>("status")
                .await
                .expect("status"),
            0
        );

        // The inhibitor and the client go with the peer that held them.
        drop(inhibitor);
        drop(presence);
        drop(sm);
        drop(client);
        let observer = zbus::connection::Builder::address(bus.address().as_str())
            .expect("address")
            .build()
            .await
            .expect("observer");
        let sm = manager_proxy(&observer, MANAGER_PATH, "org.gnome.SessionManager").await;
        let mut cleared = false;
        for _ in 0..50 {
            if !sm
                .call::<_, _, bool>("IsInhibited", &(INHIBIT_IDLE,))
                .await
                .expect("inhibited")
            {
                cleared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(cleared, "inhibitor {cookie} outlived its owner");
        assert!(sm
            .call::<_, _, Vec<OwnedObjectPath>>("GetClients", &())
            .await
            .expect("clients")
            .is_empty());

        // A forced logout needs no dialog and ends the session.
        sm.call_method("Logout", &(2_u32,)).await.expect("logout");
        tokio::time::timeout(
            Duration::from_secs(10),
            logout.wait_for(|confirmed| *confirmed),
        )
        .await
        .expect("logout in time")
        .expect("logout");
        assert!(sm.call_method("Logout", &(9_u32,)).await.is_err());
    }

    #[test]
    fn idle_delay_values_parse_from_gsettings_output() {
        assert_eq!(parse_uint32("uint32 300\n"), Some(300));
        assert_eq!(parse_uint32("idle-delay: uint32 60"), Some(60));
        assert_eq!(parse_uint32("garbage"), None);
    }

    #[test]
    fn setenv_accepts_the_display_and_refuses_what_arcen_owns() {
        assert!(setenv_allowed("DISPLAY"));
        assert!(setenv_allowed("XAUTHORITY"));
        assert!(setenv_allowed("GTK_IM_MODULE"));
        assert!(!setenv_allowed("XDG_RUNTIME_DIR"));
        assert!(!setenv_allowed("DBUS_SESSION_BUS_ADDRESS"));
        assert!(!setenv_allowed("LD_PRELOAD"));
    }
}
