//! The session manager an Arcen desktop serves on its private bus, as pure
//! state: inhibitors, registered clients, presence, logout.
//!
//! The shapes follow `org.gnome.SessionManager` as GNOME Shell and the
//! settings daemons use it; the platform adapter translates D-Bus calls
//! into these types and acts on what comes back.
//!
//! [`SessionManagerPolicy`] records where an Arcen session deliberately
//! differs from `gnome-session`.

use std::collections::BTreeMap;

/// Inhibit logging out.
pub const INHIBIT_LOGOUT: u32 = 1;
/// Inhibit user switching.
pub const INHIBIT_SWITCH_USER: u32 = 2;
/// Inhibit suspending.
pub const INHIBIT_SUSPEND: u32 = 4;
/// Inhibit the session going idle.
pub const INHIBIT_IDLE: u32 = 8;
/// Inhibit automounting.
pub const INHIBIT_AUTOMOUNT: u32 = 16;
const INHIBIT_KNOWN: u32 =
    INHIBIT_LOGOUT | INHIBIT_SWITCH_USER | INHIBIT_SUSPEND | INHIBIT_IDLE | INHIBIT_AUTOMOUNT;

/// Most inhibitors held at once.
pub const MAX_INHIBITORS: usize = 256;
/// Most registered clients.
pub const MAX_CLIENTS: usize = 256;
/// Longest application id or reason kept, in characters.
pub const MAX_TEXT_CHARS: usize = 256;

/// Where an Arcen session differs from `gnome-session`, on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionManagerPolicy {
    /// `SessionIsActive`. A remote session is not the host's console:
    /// `false` keeps the power settings daemon from dimming, blanking or
    /// suspending, and the shell from automounting the host's media. (logind
    /// reports every seatless session active, so `gnome-session` would say
    /// `true`.)
    pub session_is_active: bool,
    /// `CanShutdown`. `false`: the shell offers neither Power Off nor
    /// Restart; host power belongs to the operator.
    pub can_shutdown: bool,
    /// Whether presence changes are mirrored into logind's `IdleHint`.
    /// `false`: an idle remote session never feeds logind's `IdleAction`.
    pub sync_logind_idle_hint: bool,
}

/// The policy every Arcen session runs with.
pub const ARCEN_SESSION_MANAGER_POLICY: SessionManagerPolicy = SessionManagerPolicy {
    session_is_active: false,
    can_shutdown: false,
    sync_logind_idle_hint: false,
};

/// One inhibitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inhibitor {
    /// Application id the caller gave.
    pub app_id: String,
    /// Reason the caller gave.
    pub reason: String,
    /// Inhibit flags (known bits only).
    pub flags: u32,
    /// X11 window id the caller gave (0 on Wayland).
    pub toplevel_xid: u32,
    /// Bus unique name of the caller; the inhibitor goes when it does.
    pub owner: String,
}

/// Why an inhibitor or client request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// No flag this implementation knows was set.
    NoFlags,
    /// Too many inhibitors or clients.
    TooMany,
    /// No such cookie or client, or not the caller's.
    NotFound,
}

/// All inhibitors of the session.
#[derive(Debug, Clone, Default)]
pub struct Inhibitors {
    next_cookie: u32,
    entries: BTreeMap<u32, Inhibitor>,
}

impl Inhibitors {
    /// Adds an inhibitor and returns its cookie.
    ///
    /// # Errors
    ///
    /// [`RequestError::NoFlags`] or [`RequestError::TooMany`].
    pub fn add(
        &mut self,
        app_id: &str,
        reason: &str,
        flags: u32,
        toplevel_xid: u32,
        owner: &str,
    ) -> Result<u32, RequestError> {
        let flags = flags & INHIBIT_KNOWN;
        if flags == 0 {
            return Err(RequestError::NoFlags);
        }
        if self.entries.len() >= MAX_INHIBITORS {
            return Err(RequestError::TooMany);
        }
        let cookie = self.fresh_cookie();
        self.entries.insert(
            cookie,
            Inhibitor {
                app_id: bounded(app_id),
                reason: bounded(reason),
                flags,
                toplevel_xid,
                owner: owner.to_owned(),
            },
        );
        Ok(cookie)
    }

    /// Removes `cookie` if `caller` owns it.
    ///
    /// # Errors
    ///
    /// [`RequestError::NotFound`].
    pub fn remove(&mut self, cookie: u32, caller: &str) -> Result<Inhibitor, RequestError> {
        match self.entries.get(&cookie) {
            Some(inhibitor) if inhibitor.owner == caller => {
                self.entries.remove(&cookie).ok_or(RequestError::NotFound)
            }
            _ => Err(RequestError::NotFound),
        }
    }

    /// Removes every inhibitor `owner` holds (it left the bus); returns
    /// their cookies.
    pub fn remove_owner(&mut self, owner: &str) -> Vec<u32> {
        let cookies = self
            .entries
            .iter()
            .filter(|(_, inhibitor)| inhibitor.owner == owner)
            .map(|(cookie, _)| *cookie)
            .collect::<Vec<_>>();
        for cookie in &cookies {
            self.entries.remove(cookie);
        }
        cookies
    }

    /// The union of every inhibitor's flags (`InhibitedActions`).
    #[must_use]
    pub fn inhibited_actions(&self) -> u32 {
        self.entries
            .values()
            .fold(0, |flags, inhibitor| flags | inhibitor.flags)
    }

    /// Whether any of `flags` is inhibited.
    #[must_use]
    pub fn is_inhibited(&self, flags: u32) -> bool {
        self.inhibited_actions() & flags != 0
    }

    /// Cookies whose inhibitors hold any of `flags`.
    #[must_use]
    pub fn cookies_with(&self, flags: u32) -> Vec<u32> {
        self.entries
            .iter()
            .filter(|(_, inhibitor)| inhibitor.flags & flags != 0)
            .map(|(cookie, _)| *cookie)
            .collect()
    }

    /// Every cookie.
    #[must_use]
    pub fn cookies(&self) -> Vec<u32> {
        self.entries.keys().copied().collect()
    }

    /// One inhibitor.
    #[must_use]
    pub fn get(&self, cookie: u32) -> Option<&Inhibitor> {
        self.entries.get(&cookie)
    }

    fn fresh_cookie(&mut self) -> u32 {
        loop {
            self.next_cookie = self.next_cookie.wrapping_add(1).max(1);
            if !self.entries.contains_key(&self.next_cookie) {
                return self.next_cookie;
            }
        }
    }
}

/// A registered client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// Application id.
    pub app_id: String,
    /// The startup id it registered with (`DESKTOP_AUTOSTART_ID`).
    pub startup_id: String,
    /// Bus unique name.
    pub owner: String,
}

/// Registered clients.
#[derive(Debug, Clone, Default)]
pub struct Clients {
    next_id: u32,
    entries: BTreeMap<u32, Client>,
}

impl Clients {
    /// Registers a client and returns its number (`/org/gnome/SessionManager/Client<N>`).
    /// A caller registering again with the same startup id gets the same
    /// number.
    ///
    /// # Errors
    ///
    /// [`RequestError::TooMany`].
    pub fn register(
        &mut self,
        app_id: &str,
        startup_id: &str,
        owner: &str,
    ) -> Result<(u32, bool), RequestError> {
        if let Some((id, _)) = self
            .entries
            .iter()
            .find(|(_, client)| client.owner == owner && client.startup_id == startup_id)
        {
            return Ok((*id, false));
        }
        if self.entries.len() >= MAX_CLIENTS {
            return Err(RequestError::TooMany);
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        while self.entries.contains_key(&self.next_id) {
            self.next_id = self.next_id.wrapping_add(1).max(1);
        }
        self.entries.insert(
            self.next_id,
            Client {
                app_id: bounded(app_id),
                startup_id: bounded(startup_id),
                owner: owner.to_owned(),
            },
        );
        Ok((self.next_id, true))
    }

    /// Unregisters `id` if `caller` owns it.
    ///
    /// # Errors
    ///
    /// [`RequestError::NotFound`].
    pub fn unregister(&mut self, id: u32, caller: &str) -> Result<Client, RequestError> {
        match self.entries.get(&id) {
            Some(client) if client.owner == caller => {
                self.entries.remove(&id).ok_or(RequestError::NotFound)
            }
            _ => Err(RequestError::NotFound),
        }
    }

    /// Removes every client `owner` registered; returns their numbers.
    pub fn remove_owner(&mut self, owner: &str) -> Vec<u32> {
        let ids = self
            .entries
            .iter()
            .filter(|(_, client)| client.owner == owner)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in &ids {
            self.entries.remove(id);
        }
        ids
    }

    /// Every client number.
    #[must_use]
    pub fn ids(&self) -> Vec<u32> {
        self.entries.keys().copied().collect()
    }

    /// One client.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&Client> {
        self.entries.get(&id)
    }
}

/// `org.gnome.SessionManager.Presence` status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceStatus {
    /// 0.
    Available,
    /// 1.
    Invisible,
    /// 2.
    Busy,
    /// 3: no user activity for the idle delay.
    Idle,
}

impl PresenceStatus {
    /// The wire value.
    #[must_use]
    pub const fn value(self) -> u32 {
        match self {
            Self::Available => 0,
            Self::Invisible => 1,
            Self::Busy => 2,
            Self::Idle => 3,
        }
    }

    /// A status a client may set (`SetStatus`); idle is the session's.
    #[must_use]
    pub const fn settable(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Available),
            1 => Some(Self::Invisible),
            2 => Some(Self::Busy),
            _ => None,
        }
    }
}

/// What changes presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceEvent {
    /// The compositor's idle watch fired.
    IdleWatchFired,
    /// The user was active again.
    UserActive,
    /// A client set its status.
    SetStatus(PresenceStatus),
    /// Idle inhibitors appeared (`true`) or all went (`false`).
    IdleInhibited(bool),
    /// `org.gnome.desktop.session idle-delay` changed (seconds; 0 = never).
    IdleDelay(u32),
    /// The shell's screen shield came up (`true`) or went down (`false`):
    /// `org.gnome.ScreenSaver.ActiveChanged`, which gnome-session's presence
    /// follows. An active shield is idle; one lifted without the user
    /// touching anything (the Pier's `loginctl unlock-session` on a
    /// password-authenticated attachment) is available again, and the next
    /// idle period starts with the user's next activity.
    ScreenSaverActive(bool),
}

/// Presence: the status the shell's screen shield and app usage follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    base: PresenceStatus,
    idle: bool,
    idle_delay_secs: u32,
    idle_inhibited: bool,
    /// Left idle without activity (the shield went down): the compositor's
    /// idle time is still past the delay, so an idle watch armed now would
    /// fire at once. Until the user is active, a fired idle watch is not
    /// idleness.
    awaiting_activity: bool,
}

impl Presence {
    /// Available, with `idle_delay_secs` (0 = never idle).
    #[must_use]
    pub const fn new(idle_delay_secs: u32) -> Self {
        Self {
            base: PresenceStatus::Available,
            idle: false,
            idle_delay_secs,
            idle_inhibited: false,
            awaiting_activity: false,
        }
    }

    /// The current status.
    #[must_use]
    pub const fn status(&self) -> PresenceStatus {
        if self.idle {
            PresenceStatus::Idle
        } else {
            self.base
        }
    }

    /// The idle watch the adapter should keep armed, in seconds: `None`
    /// when the session must not go idle.
    ///
    /// The watch is persistent, as gnome-session's is: Mutter fires an idle
    /// watch once per idle period and re-arms it at the next input. A watch
    /// added (or re-added) while the user is already idle for longer fires
    /// at once, so the adapter replaces the watch only when this value
    /// changes.
    #[must_use]
    pub const fn idle_watch_secs(&self) -> Option<u32> {
        if self.idle_delay_secs == 0 || self.idle_inhibited {
            None
        } else {
            Some(self.idle_delay_secs)
        }
    }

    /// Whether the adapter should keep a user-active watch armed: while
    /// idle, and after leaving idle without activity.
    #[must_use]
    pub const fn wants_activity_watch(&self) -> bool {
        self.idle || self.awaiting_activity
    }

    /// Applies `event`; returns the new status when it changed.
    pub fn apply(&mut self, event: PresenceEvent) -> Option<PresenceStatus> {
        let before = self.status();
        match event {
            PresenceEvent::IdleWatchFired => {
                if self.idle_delay_secs != 0 && !self.idle_inhibited && !self.awaiting_activity {
                    self.idle = true;
                }
            }
            PresenceEvent::UserActive => {
                self.idle = false;
                self.awaiting_activity = false;
            }
            PresenceEvent::ScreenSaverActive(true) => {
                self.idle = true;
                self.awaiting_activity = false;
            }
            PresenceEvent::ScreenSaverActive(false) => {
                if self.idle {
                    self.idle = false;
                    self.awaiting_activity = true;
                }
            }
            PresenceEvent::SetStatus(status) => self.base = status,
            PresenceEvent::IdleInhibited(inhibited) => self.idle_inhibited = inhibited,
            PresenceEvent::IdleDelay(seconds) => {
                self.idle_delay_secs = seconds;
                if seconds == 0 {
                    self.idle = false;
                }
            }
        }
        let after = self.status();
        (after != before).then_some(after)
    }
}

/// `Logout` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoutMode {
    /// 0: ask the user (the shell's end-session dialog).
    Normal,
    /// 1: no confirmation unless something inhibits logging out.
    NoConfirmation,
    /// 2: log out regardless of inhibitors.
    Force,
}

impl LogoutMode {
    /// The mode for a wire value; unknown values are refused.
    #[must_use]
    pub const fn from_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Normal),
            1 => Some(Self::NoConfirmation),
            2 => Some(Self::Force),
            _ => None,
        }
    }

    /// Whether the shell's dialog is shown before logging out.
    #[must_use]
    pub const fn shows_dialog(self, logout_inhibited: bool) -> bool {
        match self {
            Self::Normal => true,
            Self::NoConfirmation => logout_inhibited,
            Self::Force => false,
        }
    }
}

/// How long clients get to answer `QueryEndSession`/`EndSession`, and how
/// long the shell's dialog may stay open before the logout is abandoned.
pub mod timeouts {
    use std::time::Duration;

    /// Clients' answer to `QueryEndSession` and `EndSession`.
    pub const CLIENT_RESPONSE: Duration = Duration::from_secs(5);
    /// The shell's end-session dialog counts down 60 s; a little more.
    pub const DIALOG: Duration = Duration::from_secs(75);
    /// Seconds the dialog counts down.
    pub const DIALOG_SECONDS: u32 = 60;
}

fn bounded(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .take(MAX_TEXT_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arcen_policy_never_reports_an_active_console_or_offers_power_off() {
        const {
            assert!(!ARCEN_SESSION_MANAGER_POLICY.session_is_active);
            assert!(!ARCEN_SESSION_MANAGER_POLICY.can_shutdown);
            assert!(!ARCEN_SESSION_MANAGER_POLICY.sync_logind_idle_hint);
        }
    }

    #[test]
    fn inhibitors_belong_to_their_caller_and_go_with_it() {
        let mut inhibitors = Inhibitors::default();
        assert_eq!(
            inhibitors.add("app", "why", 0x100, 0, ":1.5"),
            Err(RequestError::NoFlags)
        );
        let video = inhibitors
            .add("totem", "Playing", INHIBIT_IDLE | 0x100, 0, ":1.5")
            .expect("add");
        let logout = inhibitors
            .add("editor", "Unsaved\nwork", INHIBIT_LOGOUT, 0, ":1.6")
            .expect("add");
        assert_eq!(inhibitors.get(video).expect("video").flags, INHIBIT_IDLE);
        assert_eq!(
            inhibitors.get(logout).expect("logout").reason,
            "Unsavedwork"
        );
        assert_eq!(
            inhibitors.inhibited_actions(),
            INHIBIT_IDLE | INHIBIT_LOGOUT
        );
        assert!(inhibitors.is_inhibited(INHIBIT_IDLE));
        assert!(!inhibitors.is_inhibited(INHIBIT_SUSPEND));
        assert_eq!(inhibitors.cookies_with(INHIBIT_LOGOUT), [logout]);
        assert_eq!(
            inhibitors.remove(video, ":1.6"),
            Err(RequestError::NotFound)
        );
        assert_eq!(inhibitors.remove_owner(":1.5"), [video]);
        assert_eq!(inhibitors.inhibited_actions(), INHIBIT_LOGOUT);
        assert!(inhibitors.remove(logout, ":1.6").is_ok());
        assert_eq!(inhibitors.inhibited_actions(), 0);
        for _ in 0..MAX_INHIBITORS {
            inhibitors
                .add("a", "b", INHIBIT_IDLE, 0, ":1.7")
                .expect("add");
        }
        assert_eq!(
            inhibitors.add("a", "b", INHIBIT_IDLE, 0, ":1.7"),
            Err(RequestError::TooMany)
        );
    }

    #[test]
    fn clients_register_once_per_startup_id() {
        let mut clients = Clients::default();
        let (first, new) = clients
            .register("gsd-power", "arcen-1", ":1.9")
            .expect("register");
        assert!(new);
        assert_eq!(
            clients.register("gsd-power", "arcen-1", ":1.9"),
            Ok((first, false))
        );
        let (second, _) = clients
            .register("gsd-color", "arcen-2", ":1.10")
            .expect("register");
        assert_ne!(first, second);
        assert_eq!(
            clients.unregister(first, ":1.10"),
            Err(RequestError::NotFound)
        );
        assert_eq!(clients.remove_owner(":1.10"), [second]);
        assert_eq!(clients.ids(), [first]);
    }

    #[test]
    fn presence_goes_idle_only_when_allowed_and_wakes_on_activity() {
        let mut presence = Presence::new(300);
        assert_eq!(presence.idle_watch_secs(), Some(300));
        assert_eq!(
            presence.apply(PresenceEvent::IdleWatchFired),
            Some(PresenceStatus::Idle)
        );
        // The watch stays armed; Mutter re-arms it at the next input.
        assert_eq!(presence.idle_watch_secs(), Some(300));
        assert_eq!(
            presence.apply(PresenceEvent::UserActive),
            Some(PresenceStatus::Available)
        );

        assert_eq!(presence.apply(PresenceEvent::IdleInhibited(true)), None);
        assert_eq!(presence.idle_watch_secs(), None);
        assert_eq!(presence.apply(PresenceEvent::IdleWatchFired), None);
        assert_eq!(presence.status(), PresenceStatus::Available);
        presence.apply(PresenceEvent::IdleInhibited(false));

        assert_eq!(presence.apply(PresenceEvent::IdleDelay(0)), None);
        assert_eq!(presence.idle_watch_secs(), None);
        assert_eq!(presence.apply(PresenceEvent::IdleWatchFired), None);

        presence.apply(PresenceEvent::IdleDelay(60));
        presence.apply(PresenceEvent::SetStatus(PresenceStatus::Busy));
        assert_eq!(presence.status(), PresenceStatus::Busy);
        presence.apply(PresenceEvent::IdleWatchFired);
        assert_eq!(presence.status(), PresenceStatus::Idle);
        assert_eq!(
            presence.apply(PresenceEvent::UserActive),
            Some(PresenceStatus::Busy)
        );
        assert_eq!(PresenceStatus::settable(3), None);
        assert_eq!(PresenceStatus::Idle.value(), 3);
    }

    #[test]
    fn presence_follows_the_screen_shield_like_gnome_session() {
        let mut presence = Presence::new(300);
        presence.apply(PresenceEvent::IdleWatchFired);
        // The shell locks on idle; still idle.
        assert_eq!(presence.apply(PresenceEvent::ScreenSaverActive(true)), None);
        // The Pier unlocks on a password-authenticated attachment.
        assert_eq!(
            presence.apply(PresenceEvent::ScreenSaverActive(false)),
            Some(PresenceStatus::Available)
        );
        // A watch that fires before the user is active (one re-armed while
        // the compositor's idle time is past the delay fires at once) is not
        // idleness: it would lock the desktop the Pier just unlocked.
        assert!(presence.wants_activity_watch());
        assert_eq!(presence.apply(PresenceEvent::IdleWatchFired), None);
        assert_eq!(presence.status(), PresenceStatus::Available);
        assert_eq!(presence.apply(PresenceEvent::UserActive), None);
        assert_eq!(presence.idle_watch_secs(), Some(300));
        assert!(!presence.wants_activity_watch());

        // A manual lock (Super+L) while active is idle, as on a GDM login.
        assert_eq!(
            presence.apply(PresenceEvent::ScreenSaverActive(true)),
            Some(PresenceStatus::Idle)
        );
        assert!(presence.wants_activity_watch());
        // An unlock typed in the dialog is activity too.
        assert_eq!(
            presence.apply(PresenceEvent::UserActive),
            Some(PresenceStatus::Available)
        );
        assert_eq!(
            presence.apply(PresenceEvent::ScreenSaverActive(false)),
            None
        );
        assert_eq!(presence.idle_watch_secs(), Some(300));
    }

    #[test]
    fn logout_modes_decide_the_dialog() {
        assert!(LogoutMode::Normal.shows_dialog(false));
        assert!(!LogoutMode::NoConfirmation.shows_dialog(false));
        assert!(LogoutMode::NoConfirmation.shows_dialog(true));
        assert!(!LogoutMode::Force.shows_dialog(true));
        assert_eq!(LogoutMode::from_value(3), None);
    }
}
