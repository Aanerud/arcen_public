// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Arcen joins the user's session (ADR 0025).
//!
//! "Arcen is an extension of your machine — not a locked-in new desktop
//! every time." A Pier serves the graphical session the authenticated user
//! already holds at the workstation, and a remote-first sign-in creates the
//! operating system's own session (one the login screen later resumes), so
//! there is one session per user, entered from either side.
//!
//! This module holds every decision of that model that does not need the
//! operating system: which console session a sign-in joins, when it must be
//! started through the login screen, who is refused and with which code,
//! what happens to the local console while a Deck is attached and when it
//! leaves, and the one-time token that completes a remote-first sign-in
//! through the login screen. The Linux Pier gathers the facts (logind, the
//! session's processes, evdev, `RandR`) and acts on the answers; the Windows
//! and macOS Piers reach the same model through their own console APIs.

pub mod account_desktop;
pub mod desk_privacy;
pub mod detached_hold;
pub mod domain_logon;
pub mod foreign_service;
pub mod greeter_token;
pub mod local_login_screen;
pub mod lock_wait;
pub mod login_screen;
pub mod login_session;
pub mod logind_read;
pub mod owner_death;
pub mod session_watch;
pub mod stream_end;
pub mod wayland_attach;
pub mod wayland_join;

pub use foreign_service::{ConsoleNotice, ForeignService, other_product_holder};
pub use local_login_screen::LoginScreen;
pub use logind_read::{SessionRead, SessionsUnreadable};

/// How a Linux Pier serves a sign-in (`platform.session.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesktopMode {
    /// Join the user's console session; a remote-first sign-in goes through
    /// the display manager's login screen, or, on a host without one,
    /// starts a real login session ([`login_session`]). The default.
    #[default]
    Console,
    /// A private Arcen desktop per sign-in, sized and arranged to the Deck
    /// (one Arcen head per Deck display). An explicit administrator opt-in,
    /// never a default: a host without a display manager gets a real,
    /// persistent login session in `Console` mode (ADR 0025 §10). It is
    /// kept only because it is the one mode whose X server's heads follow
    /// the Deck's displays; a joined console keeps the host's own monitors.
    Dedicated,
}

impl DesktopMode {
    /// Every mode, in configuration order.
    pub const ALL: [Self; 2] = [Self::Console, Self::Dedicated];

    /// The configuration token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Dedicated => "dedicated",
        }
    }

    /// Parses [`Self::token`].
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.token() == value)
    }
}

/// The logind seat every console session lives on.
pub const CONSOLE_SEAT: &str = "seat0";

/// The logind TTY every Arcen dedicated desktop is opened with: such a
/// desktop is never a console session.
pub const ARCEN_TTY: &str = "arcen";

/// Who started a console session (for logs and the refusals; it never
/// changes what the Pier does to a session it joins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleOrigin {
    /// The display manager (GDM: `Service=gdm-*`), from the keyboard or
    /// through Arcen's remote-first sign-in. It manages the console's
    /// virtual terminals and resumes the sessions it started.
    DisplayManager,
    /// Anything else on the console seat: another remote-desktop product
    /// that runs its own display manager and X server, `startx` from a
    /// text console. The display manager cannot resume or switch it.
    OtherProduct,
    /// Arcen's own login session on a host without a display manager
    /// ([`login_session`], `Service=arcen-login`).
    Arcen,
}

impl ConsoleOrigin {
    fn from_service(service: &str) -> Self {
        if service.starts_with("gdm-") {
            Self::DisplayManager
        } else if service == login_session::PAM_SERVICE {
            Self::Arcen
        } else {
            Self::OtherProduct
        }
    }

    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::DisplayManager => "display_manager",
            Self::OtherProduct => "other_product",
            Self::Arcen => "arcen",
        }
    }
}

/// One logind session, as `loginctl show-session` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsoleSessionFact {
    pub id: String,
    pub uid: u32,
    pub user: String,
    pub seat: String,
    pub vtnr: u32,
    /// The PAM TTY item (`arcen` for an Arcen dedicated desktop).
    pub tty: String,
    /// `user`, `greeter`, `lock-screen`, `background`, `manager`…
    pub class: String,
    /// `x11`, `wayland`, `tty`, `unspecified`…
    pub session_type: String,
    /// `online`, `active` or `closing`.
    pub state: String,
    pub active: bool,
    pub remote: bool,
    /// The PAM service that opened it (`gdm-password`, `gdm-ovirtcred`…).
    pub service: String,
    pub locked: bool,
    /// The process that opened the session (logind `Leader=`; 0 unknown).
    pub leader: u32,
    /// logind's `Desktop=` (`gnome`…; empty when the display manager did
    /// not name one). Evidence for logs, never a decision.
    pub desktop: String,
}

impl ConsoleSessionFact {
    /// Parses `key=value` lines as printed by `loginctl show-session`.
    /// Unknown keys are ignored; a session without an `Id` or a numeric
    /// `User` is not a fact anything can be decided on.
    #[must_use]
    pub fn from_properties(properties: &str) -> Option<Self> {
        let mut fact = Self::default();
        let mut uid = None;
        for line in properties.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key {
                "Id" => value.clone_into(&mut fact.id),
                "User" => uid = value.parse::<u32>().ok(),
                "Name" => value.clone_into(&mut fact.user),
                "Seat" => value.clone_into(&mut fact.seat),
                "VTNr" => fact.vtnr = value.parse().unwrap_or(0),
                "TTY" => value.clone_into(&mut fact.tty),
                "Class" => value.clone_into(&mut fact.class),
                "Type" => value.clone_into(&mut fact.session_type),
                "State" => value.clone_into(&mut fact.state),
                "Active" => fact.active = value == "yes",
                "Remote" => fact.remote = value == "yes",
                "Service" => value.clone_into(&mut fact.service),
                "LockedHint" => fact.locked = value == "yes",
                "Leader" => fact.leader = value.parse().unwrap_or(0),
                "Desktop" => value.clone_into(&mut fact.desktop),
                _ => {}
            }
        }
        fact.uid = uid?;
        (!fact.id.is_empty()).then_some(fact)
    }

    /// On the console seat and not closing. An Arcen dedicated desktop
    /// (`TTY=arcen`) is never a console; another product's desktop there is,
    /// whatever remote host its PAM conversation named.
    fn on_console(&self) -> bool {
        self.seat == CONSOLE_SEAT && self.tty != ARCEN_TTY && self.state != "closing"
    }

    fn graphical(&self) -> bool {
        matches!(self.session_type.as_str(), "x11" | "wayland")
    }

    /// A user's graphical session on the console seat.
    #[must_use]
    pub fn is_console_desktop(&self) -> bool {
        self.on_console() && self.graphical() && self.class == "user"
    }

    /// The display manager's login screen on the console seat.
    #[must_use]
    pub fn is_console_greeter(&self) -> bool {
        self.on_console() && self.graphical() && self.class == "greeter"
    }

    /// Who started it.
    #[must_use]
    pub fn origin(&self) -> ConsoleOrigin {
        ConsoleOrigin::from_service(&self.service)
    }

    fn is_wayland(&self) -> bool {
        self.session_type == "wayland"
    }
}

/// What a sign-in on a console host does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleDecision {
    /// Join this session of the authenticated user. `activate` asks logind
    /// to bring it to the foreground first (it waits behind the login
    /// screen, as after "Switch User"); the Pier proved the user's password,
    /// which is what the login screen would have asked for.
    Attach {
        session: String,
        activate: bool,
        origin: ConsoleOrigin,
    },
    /// Join this session of the authenticated user, which runs GNOME on
    /// Wayland: the Pier proves its compositor read-only and asks
    /// [`wayland_join::decide_join`]. A Pier without the Wayland console
    /// adapter refuses visibly (`console_wayland_session`), with the
    /// evidence in its log (GAP-042).
    JoinWayland(wayland_join::WaylandTarget),
    /// Nobody holds the console and the login screen is showing: complete
    /// the sign-in through it, so the display manager creates (or, when the
    /// user already has a session it started, resumes) the session.
    SignInThroughGreeter { greeter: String },
    /// Nobody holds the console and the host has no display manager: Arcen
    /// signs the account in itself, a real login session on `seat0` at
    /// virtual terminal `vt` ([`login_session`]).
    StartLoginSession { vt: u32 },
    /// The sign-in is refused, with a code the Deck shows.
    Refuse(ConsoleRefusal),
}

/// What the host has beside its logind sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsoleHost {
    /// A display manager is configured (`display-manager.service`): its
    /// login screen completes a remote-first sign-in, and its absence on
    /// `seat0` is a failure, not an invitation.
    pub display_manager: bool,
    /// Whether that display manager's login screen may be on the console:
    /// [`LoginScreen::Withheld`] when another product is the display
    /// manager and runs GDM without its local login screen.
    pub login_screen: LoginScreen,
}

impl Default for ConsoleHost {
    fn default() -> Self {
        Self {
            display_manager: true,
            login_screen: LoginScreen::Shown,
        }
    }
}

/// Display managers whose login screen completes a remote-first sign-in or
/// comes back by itself: a configured one that is down is a failure
/// (`console_no_session`), not a host without one.
pub const DISPLAY_MANAGER_UNITS: &[&str] = &[
    "gdm.service",
    "gdm3.service",
    "lightdm.service",
    "sddm.service",
    "lxdm.service",
    "xdm.service",
    "slim.service",
];

impl ConsoleHost {
    /// The host's display manager from what systemd says about
    /// `display-manager.service`: the unit the alias points at (`None`
    /// when there is no alias) and whether it is running.
    ///
    /// A display manager's alias counts whether or not it runs. An alias to
    /// anything else — another remote-desktop product that makes itself the
    /// display manager and starts a display manager only while it runs —
    /// counts only while that unit runs: stopped, nothing will show a login
    /// screen, and the host is served like one without a display manager
    /// (ADR 0025 §10).
    #[must_use]
    pub fn from_display_manager(alias_target: Option<&str>, active: bool) -> Self {
        let display_manager = match alias_target {
            None => false,
            Some(unit) => active || DISPLAY_MANAGER_UNITS.contains(&unit),
        };
        Self {
            display_manager,
            login_screen: LoginScreen::Shown,
        }
    }

    /// [`Self::from_display_manager`], with whether GDM shows its local
    /// login screen ([`local_login_screen::gdm_shows_local_login_screen`]),
    /// asked only when another product's running unit is the display
    /// manager ([`LoginScreen::of`]).
    #[must_use]
    pub fn from_display_manager_and_gdm(
        alias_target: Option<&str>,
        active: bool,
        gdm_shows_local: impl FnOnce() -> Option<bool>,
    ) -> Self {
        let mut host = Self::from_display_manager(alias_target, active);
        host.login_screen = LoginScreen::of(alias_target, host.display_manager, gdm_shows_local);
        host
    }
}

/// Decides what a sign-in of `uid` does on a host with a display manager.
#[must_use]
pub fn decide(sessions: &[ConsoleSessionFact], uid: u32) -> ConsoleDecision {
    decide_on(sessions, uid, ConsoleHost::default())
}

/// Decides what a sign-in of `uid` does, from every logind session.
///
/// The user's own console desktop is joined (an X11 one preferred; a
/// Wayland one is the [`ConsoleDecision::JoinWayland`] target); one held by
/// another account (actively, or outside the display manager in any state)
/// refuses; with only the login screen showing the sign-in goes through it
/// (a Wayland login screen refuses until Arcen signs in through one); with
/// nothing on the console seat there is nothing to join and the sign-in is
/// refused rather than given a private desktop.
///
/// With nothing on the console seat, a host without a display manager gets
/// Arcen's own login session for the account; one with a display manager
/// whose login screen is missing refuses. Where another product runs the
/// console's sign-in ([`LoginScreen::Withheld`]) nothing is signed in
/// through a login screen, not even one left on the seat: the sign-in is
/// refused with [`ConsoleRefusal::SignInOtherProduct`].
#[must_use]
pub fn decide_on(sessions: &[ConsoleSessionFact], uid: u32, host: ConsoleHost) -> ConsoleDecision {
    let desktops = || sessions.iter().filter(|s| s.is_console_desktop());
    if let Some(active) = desktops().find(|s| s.active) {
        if active.uid != uid {
            return ConsoleDecision::Refuse(ConsoleRefusal::other_user(active.origin()));
        }
        return attach(active, false);
    }
    // Another account's session the display manager did not start keeps
    // the console even while it is not in the foreground: the display
    // manager cannot switch it away, and a second X server must not start
    // beside it on the same GPU.
    if desktops().any(|s| s.uid != uid && s.origin() == ConsoleOrigin::OtherProduct) {
        return ConsoleDecision::Refuse(ConsoleRefusal::OtherUserOtherProduct);
    }
    // Arcen's own login session of another account keeps the workstation
    // too: without a display manager there is no switching between them,
    // and the Pier serves one desktop at a time.
    if desktops().any(|s| s.uid != uid && s.origin() == ConsoleOrigin::Arcen) {
        return ConsoleDecision::Refuse(ConsoleRefusal::OtherUser);
    }
    let mine = desktops()
        .filter(|s| s.uid == uid)
        .min_by_key(|s| (s.is_wayland(), s.state != "online"));
    if let Some(mine) = mine {
        return attach(mine, true);
    }
    if host.login_screen.refuses_sign_in() {
        return ConsoleDecision::Refuse(ConsoleRefusal::SignInOtherProduct);
    }
    match sessions.iter().find(|s| s.is_console_greeter()) {
        Some(greeter) if greeter.is_wayland() => {
            ConsoleDecision::Refuse(ConsoleRefusal::WaylandSession)
        }
        Some(greeter) => ConsoleDecision::SignInThroughGreeter {
            greeter: greeter.id.clone(),
        },
        None if host.display_manager => ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession),
        None => {
            let used: Vec<u32> = sessions
                .iter()
                .filter(|s| s.seat == CONSOLE_SEAT && s.state != "closing")
                .map(|s| s.vtnr)
                .collect();
            login_session::choose_vt(&used).map_or(
                ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession),
                |vt| ConsoleDecision::StartLoginSession { vt },
            )
        }
    }
}

/// [`decide_on`] from a read of logind's sessions: a read that is not
/// complete ([`SessionsUnreadable`]) refuses
/// ([`ConsoleRefusal::SessionsUnreadable`]). It is never "nobody holds the
/// console": on a host without a display manager that would start a
/// second login session for an account that may already hold one. Only a
/// complete read proves that the account has no session.
#[must_use]
pub fn decide_on_read(read: &SessionRead, uid: u32, host: ConsoleHost) -> ConsoleDecision {
    match read {
        Ok(sessions) => decide_on(sessions, uid, host),
        Err(_) => ConsoleDecision::Refuse(ConsoleRefusal::SessionsUnreadable),
    }
}

/// What a returning Deck of `uid` may do with the console session `held`
/// its attachment joined: `Ok(activate)` when the console still attaches
/// exactly that session of the same account (`activate` brings it forward
/// from behind a login screen), otherwise the refusal the Deck is shown.
///
/// # Errors
///
/// The console's own refusal (another account took the workstation, it
/// runs Wayland now…), or [`ConsoleRefusal::SessionChanged`] when the
/// account's session is no longer the one this attachment joined.
pub fn decide_reattach(
    sessions: &[ConsoleSessionFact],
    uid: u32,
    held: &str,
    host: ConsoleHost,
) -> Result<bool, ConsoleRefusal> {
    let owned = sessions
        .iter()
        .any(|fact| fact.id == held && fact.uid == uid);
    match decide_on(sessions, uid, host) {
        ConsoleDecision::Attach {
            session, activate, ..
        } if owned && session == held => Ok(activate),
        ConsoleDecision::Refuse(refusal) => Err(refusal),
        // A held attachment is an X11 one until the Wayland console adapter
        // exists: the account's Wayland session is refused as at sign-in.
        ConsoleDecision::JoinWayland(_) => Err(ConsoleRefusal::WaylandSession),
        ConsoleDecision::Attach { .. }
        | ConsoleDecision::SignInThroughGreeter { .. }
        | ConsoleDecision::StartLoginSession { .. } => Err(ConsoleRefusal::SessionChanged),
    }
}

/// [`decide_reattach`] from a read of logind's sessions.
///
/// # Errors
///
/// [`ConsoleRefusal::SessionsUnreadable`] when the read is not complete,
/// otherwise as [`decide_reattach`].
pub fn decide_reattach_read(
    read: &SessionRead,
    uid: u32,
    held: &str,
    host: ConsoleHost,
) -> Result<bool, ConsoleRefusal> {
    match read {
        Ok(sessions) => decide_reattach(sessions, uid, held, host),
        Err(_) => Err(ConsoleRefusal::SessionsUnreadable),
    }
}

/// Where a remote-first sign-in through the login screen `greeter` stands,
/// seen from every logind session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInSettlement {
    /// The display manager has not finished: no session yet, or the login
    /// screen that took the sign-in is still on the seat.
    Pending,
    /// The user's session is in the foreground and the login screen that
    /// took the sign-in is gone.
    Settled(String),
    /// The login screen finished, but the display manager put a new one in
    /// the foreground in front of the user's session. (GDM does this when
    /// its login screen ends while it still counts the login VT as active;
    /// the race is lost more often on a slow mode set.) The user's session
    /// is brought forward again, as the person who signed in would.
    BringForward(String),
}

/// Decides the next step of a remote-first sign-in of `uid` handed to the
/// login screen session `greeter`.
#[must_use]
pub fn settle_sign_in(
    sessions: &[ConsoleSessionFact],
    uid: u32,
    greeter: &str,
) -> SignInSettlement {
    let Some(mine) = sessions
        .iter()
        .filter(|s| s.uid == uid && s.is_console_desktop())
        .min_by_key(|s| !s.active)
    else {
        return SignInSettlement::Pending;
    };
    // A login screen that is closing is finished: only processes it left
    // behind keep its session (an `ibus-daemon` of a crashed shell).
    if sessions
        .iter()
        .any(|s| s.id == greeter && s.state != "closing")
    {
        return SignInSettlement::Pending;
    }
    if mine.active {
        return SignInSettlement::Settled(mine.id.clone());
    }
    let new_login_screen_in_front = sessions.iter().any(|s| s.is_console_greeter() && s.active);
    let nobody_else_in_front = !sessions.iter().any(|s| s.is_console_desktop() && s.active);
    if new_login_screen_in_front && nobody_else_in_front {
        SignInSettlement::BringForward(mine.id.clone())
    } else {
        SignInSettlement::Pending
    }
}

fn attach(session: &ConsoleSessionFact, activate: bool) -> ConsoleDecision {
    if session.is_wayland() {
        return ConsoleDecision::JoinWayland(wayland_join::WaylandTarget::of(session, activate));
    }
    ConsoleDecision::Attach {
        session: session.id.clone(),
        activate,
        origin: session.origin(),
    }
}

/// Why a console sign-in is refused. The Deck shows [`Self::deck_reason`];
/// the Pier logs [`Self::token`] as the reason class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleRefusal {
    /// Another account holds the workstation's session, started by the
    /// display manager. Arcen never takes another account's session over.
    OtherUser,
    /// Another account holds the workstation through a session the display
    /// manager did not start (another remote-desktop product with its own
    /// X server, or `startx`), active or in the background.
    OtherUserOtherProduct,
    /// The session (or the login screen's next session) is GNOME on
    /// Wayland, and this Pier cannot join it yet: the account's own
    /// session is identified ([`wayland_join::WaylandJoin::AdapterUnavailable`],
    /// its evidence in the Pier's log) but not joined (GAP-042).
    WaylandSession,
    /// Nobody is signed in and no login screen runs on the console: there
    /// is no session to join and none can be created through the display
    /// manager.
    NoConsoleSession,
    /// logind's sessions could not be read completely (it did not answer,
    /// or not about every session, or contradicted itself): who is signed
    /// in is unknown, so nothing is joined and no session is started.
    SessionsUnreadable,
    /// The login screen cannot complete a remote sign-in on this host: the
    /// installer's sign-in components are missing or another service owns
    /// the login screen's credential channel.
    SignInUnavailable,
    /// The login screen did not start the session in time, or refused it
    /// (account policy, home directory, a display-manager error).
    SignInFailed,
    /// The session's display could not be found or opened.
    SessionUnreachable,
    /// The local monitors could not be blanked or the local input disabled;
    /// the session is not shown remotely while the desk could watch it.
    PrivacyUnavailable,
    /// A returning Deck's session is no longer the account's session at the
    /// workstation (the user signed out, or signed in afresh at the desk);
    /// a new sign-in joins the current one.
    SessionChanged,
    /// The password was right, but the host's account policy (for a domain
    /// account, its domain policy: Group Policy's "Allow log on locally")
    /// does not let this account sign in at this workstation.
    AccountDenied,
    /// The account's own desktop in another remote-desktop product is in
    /// use through that product right now (its client is connected). One
    /// remote product at a time: Arcen does not take the desktop over, and
    /// ends its own attachment when that product's client connects.
    InUseOtherProduct,
    /// While a Deck was attached, another remote-desktop product started its
    /// own desktop on the console (it is the host's display manager again).
    /// Arcen steps aside rather than fight it for the GPU and the virtual
    /// terminal; the account's Arcen desktop is not ended.
    OtherProductStarted,
    /// Nobody is signed in, and another remote-desktop product runs the
    /// workstation's sign-in: it is the display manager and keeps GDM's
    /// login screen off the console ([`LoginScreen::Withheld`]). Signing
    /// in with that product once gives the account a desktop Arcen joins.
    SignInOtherProduct,
}

impl ConsoleRefusal {
    /// Every refusal (the error-code reference checks each has a row).
    pub const ALL: [Self; 14] = [
        Self::OtherUser,
        Self::OtherUserOtherProduct,
        Self::WaylandSession,
        Self::NoConsoleSession,
        Self::SessionsUnreadable,
        Self::SignInUnavailable,
        Self::SignInFailed,
        Self::SessionUnreachable,
        Self::PrivacyUnavailable,
        Self::SessionChanged,
        Self::AccountDenied,
        Self::InUseOtherProduct,
        Self::OtherProductStarted,
        Self::SignInOtherProduct,
    ];

    /// The refusal for another account's session of `origin`.
    #[must_use]
    pub const fn other_user(origin: ConsoleOrigin) -> Self {
        match origin {
            ConsoleOrigin::DisplayManager | ConsoleOrigin::Arcen => Self::OtherUser,
            ConsoleOrigin::OtherProduct => Self::OtherUserOtherProduct,
        }
    }

    /// Stable code: the reason class in the Pier's log and the first word
    /// of what the Deck shows.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::OtherUser => "console_other_user",
            Self::OtherUserOtherProduct => "console_other_user_other_product",
            Self::WaylandSession => "console_wayland_session",
            Self::NoConsoleSession => "console_no_session",
            Self::SessionsUnreadable => "console_sessions_unreadable",
            Self::SignInUnavailable => "console_sign_in_unavailable",
            Self::SignInFailed => "console_sign_in_failed",
            Self::SessionUnreachable => "console_session_unreachable",
            Self::PrivacyUnavailable => "console_privacy_unavailable",
            Self::SessionChanged => "console_session_changed",
            Self::AccountDenied => "console_account_denied",
            Self::InUseOtherProduct => "console_in_use_other_product",
            Self::OtherProductStarted => "console_other_product_started",
            Self::SignInOtherProduct => "console_sign_in_other_product",
        }
    }

    /// What the Deck shows (a close reason: at most 120 bytes), led by the
    /// code so a support request can quote it.
    #[must_use]
    pub const fn deck_reason(self) -> &'static str {
        match self {
            Self::OtherUser => {
                "console_other_user: another account is signed in at this workstation."
            }
            Self::OtherUserOtherProduct => {
                "console_other_user_other_product: another account uses this workstation through another remote-desktop product."
            }
            Self::WaylandSession => {
                "console_wayland_session: this workstation runs GNOME on Wayland; Arcen joins X11 sessions."
            }
            Self::NoConsoleSession => {
                "console_no_session: nobody is signed in and the workstation's login screen is not running."
            }
            Self::SessionsUnreadable => {
                "console_sessions_unreadable: the workstation could not say who is signed in; no session was joined or started."
            }
            Self::SignInUnavailable => {
                "console_sign_in_unavailable: this workstation's login screen cannot take a remote sign-in."
            }
            Self::SignInFailed => {
                "console_sign_in_failed: the workstation's login screen did not start your session."
            }
            Self::SessionUnreachable => {
                "console_session_unreachable: your session's display could not be opened."
            }
            Self::PrivacyUnavailable => {
                "console_privacy_unavailable: the workstation's screens, speakers and input could not be made private."
            }
            Self::SessionChanged => {
                "console_session_changed: your session at this workstation changed; sign in again to join it."
            }
            Self::AccountDenied => {
                "console_account_denied: your (domain) account policy does not allow this account to sign in at this workstation."
            }
            Self::InUseOtherProduct => {
                "console_in_use_other_product: this desktop is in use through another remote-desktop product."
            }
            Self::OtherProductStarted => {
                "console_other_product_started: another remote-desktop product took over this workstation."
            }
            Self::SignInOtherProduct => {
                "console_sign_in_other_product: another remote-desktop product runs this workstation's sign-in; sign in with it first."
            }
        }
    }
}

/// The kernel's bus type for host-platform devices (`BUS_HOST` in
/// `linux/input.h`): firmware hotkeys, ACPI buttons and the like.
pub const BUS_HOST: u16 = 0x19;

/// The kernel's bus type for Bluetooth devices (`BUS_BLUETOOTH`). The desk's
/// Bluetooth LE keyboards and mice are made through uhid by the Bluetooth
/// daemon, so they have no parent device either.
pub const BUS_BLUETOOTH: u16 = 0x05;

/// One evdev input device, as the Pier sees it before deciding whether a
/// person at the desk could use it while a Deck is attached.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts about one device, each read from sysfs or udev"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputDeviceFact {
    /// Without a parent device (`/sys/devices/virtual`): created through
    /// uinput or uhid (Arcen's own virtual devices, another remote
    /// product's), or registered by a firmware driver without one (HP's WMI
    /// hotkeys).
    pub virtual_device: bool,
    /// On the console seat (no `ID_SEAT`, or `seat0`).
    pub console_seat: bool,
    /// A Deck's bridged USB device (Native tablet, ADR 0021).
    pub bridged: bool,
    /// Keyboard, pointer, touch, tablet or keys: something a person can
    /// drive the desktop with.
    pub drives_desktop: bool,
    /// A keyboard or a pointer (mouse, touchpad, tablet, touchscreen,
    /// pointing stick, trackball) as udev classifies it, not a device with
    /// only a few keys, buttons or switches (hotkeys, a power button).
    pub keyboard_or_pointer: bool,
    /// The device's bus type (`id/bustype`).
    pub bus: u16,
}

/// Who holds the kernel's interfaces for input devices made in user space
/// (`/dev/uinput`, `/dev/uhid`) open. A device made through them exists
/// only while its creator holds one, so only the holders of the interface
/// that made a device ([`UserSpaceInputInterface::of_device`],
/// [`HeldInterfaces::could_make`]) are asked about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserSpaceInputCreators {
    /// Nobody, or only Arcen's own processes and drivers for hardware at
    /// the desk ([`local_input_driver`]).
    ArcenOnly,
    /// A process that is neither.
    Other,
    /// Could not be read.
    Unknown,
}

/// Executables (by file name) of system services that make devices
/// through `/dev/uinput` or `/dev/uhid` only for hardware at the desk, and
/// offer no other program a way to type or point through them (no
/// injection command, no plugins). Their devices never mean another
/// product's client is using the desktop. A tool that another program can
/// drive (an input-automation daemon, a remapper with a text or macro
/// command, a streaming client) is never listed: a remote product could
/// type through it. The reasoning is ADR 0025 §8 (amended for 0.21.2).
pub const LOCAL_INPUT_DRIVERS: [&str; 4] = [
    // A colour-grading panel's service: the panel's own keyboard and mouse.
    "TangentHub",
    // BlueZ: Bluetooth keyboards and mice through uhid, always on the
    // Bluetooth bus (src/shared/uhid.c).
    "bluetoothd",
    // A mouse and keyboard configuration daemon: the mouse's remapped
    // buttons and gestures; its D-Bus interface configures, never types.
    "logid",
    // A key remapper: re-emits the desk's keyboard; its control socket
    // switches modes only.
    "xremap",
];

/// The kernel's interfaces for input devices made in user space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserSpaceInputInterface {
    /// `/dev/uinput`: the device has no parent at all
    /// (`/sys/devices/virtual/input/inputN`).
    Uinput,
    /// `/dev/uhid`: the device's parent is the uhid misc device
    /// (`/sys/devices/virtual/misc/uhid/…`).
    Uhid,
}

impl UserSpaceInputInterface {
    /// Which interface could have made the input device at
    /// `sysfs_device` (its `/sys/class/input/eventN/device`, resolved).
    /// `None`: any of them (a parentless device elsewhere).
    ///
    /// A device made through one interface exists only while a process
    /// holds that interface open, so a process holding only the other
    /// cannot be its creator: bluetoothd holding `/dev/uhid` for the desk's
    /// Bluetooth keyboard does not make a uinput device another product's.
    /// A kernel driver's parentless device also has the uinput shape; it is
    /// treated as uinput's, as before.
    #[must_use]
    pub fn of_device(sysfs_device: &str) -> Option<Self> {
        if sysfs_device.starts_with("/sys/devices/virtual/misc/uhid/") {
            return Some(Self::Uhid);
        }
        let node = sysfs_device.strip_prefix("/sys/devices/virtual/input/")?;
        (node.starts_with("input") && !node.contains('/')).then_some(Self::Uinput)
    }
}

/// Which user-space input interfaces one process holds open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeldInterfaces {
    pub uinput: bool,
    pub uhid: bool,
}

impl HeldInterfaces {
    /// Whether a process holding these could have made a device made
    /// through `interface` (`None`: through any of them).
    #[must_use]
    pub const fn could_make(self, interface: Option<UserSpaceInputInterface>) -> bool {
        match interface {
            Some(UserSpaceInputInterface::Uinput) => self.uinput,
            Some(UserSpaceInputInterface::Uhid) => self.uhid,
            None => self.uinput || self.uhid,
        }
    }
}

/// The owner and mode of the file a process runs (as `stat` reports them
/// for the running file itself, not for a path that may since name
/// another).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutableOwner {
    pub uid: u32,
    pub gid: u32,
    /// `st_mode`, file type bits included.
    pub mode: u32,
}

impl ExecutableOwner {
    /// A regular file owned by root that only root can change (no write
    /// bit for others, and for its group only when that group is root's).
    #[must_use]
    pub const fn root_only(self) -> bool {
        self.mode & 0o170_000 == 0o100_000
            && self.uid == 0
            && self.mode & 0o002 == 0
            && (self.mode & 0o020 == 0 || self.gid == 0)
    }
}

/// What the Pier read about a process holding a user-space input interface
/// whose executable names a [`LOCAL_INPUT_DRIVERS`] entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDriverFacts {
    /// All of its user IDs (real, effective, saved, file system) are root's.
    pub runs_as_root: bool,
    /// The running file's owner and mode (`None`: unreadable).
    pub executable: Option<ExecutableOwner>,
}

/// Whether `executable_path`'s file name is a [`LOCAL_INPUT_DRIVERS`] entry
/// (a ` (deleted)` suffix after an update ignored). Names only: the Pier
/// reads [`LocalDriverFacts`] for such a process and asks
/// [`local_input_driver`].
#[must_use]
pub fn local_input_driver_name(executable_path: &str) -> bool {
    let path = executable_path
        .strip_suffix(" (deleted)")
        .unwrap_or(executable_path);
    let name = path.rsplit('/').next().unwrap_or(path);
    LOCAL_INPUT_DRIVERS.contains(&name)
}

/// Whether the process whose executable is `executable_path` is a driver
/// for hardware at the desk: its file name is a [`LOCAL_INPUT_DRIVERS`]
/// entry, it runs as root, and the file it runs is root's and writable by
/// root only. The name alone is never enough: anyone can name a program
/// `TangentHub`. A program the session's user started or installed (in a
/// home directory, under a name of their choosing, open to their own
/// programs' tracing) never counts as a driver at the desk.
#[must_use]
pub fn local_input_driver(executable_path: &str, facts: LocalDriverFacts) -> bool {
    local_input_driver_name(executable_path)
        && facts.runs_as_root
        && facts.executable.is_some_and(ExecutableOwner::root_only)
}

impl UserSpaceInputCreators {
    /// Whether a device made in user space may be another product's: a
    /// creator outside Arcen exists, or none could be ruled out (unknown
    /// counts as one, so a failed read never lets Arcen join a desktop
    /// another product's client is using).
    #[must_use]
    pub const fn include_another_product(self) -> bool {
        !matches!(self, Self::ArcenOnly)
    }
}

/// Whether the device is the desk's own input, disabled while a Deck is
/// attached. Software devices are never touched: Arcen's own injectors
/// must keep working, and another remote product's are not the desk's.
#[must_use]
pub const fn disables_while_attached(device: InputDeviceFact) -> bool {
    device.drives_desktop && device.console_seat && !device.virtual_device && !device.bridged
}

/// Whether the device, by its own facts, could be a remote client's input
/// of another product: a keyboard or pointer on the console seat without a
/// parent device, neither on the host-platform bus nor a Bluetooth device
/// at the desk, that is not Arcen's own (`arcen_device`: Arcen's virtual
/// input vendor) nor a Deck's bridged device. A firmware driver's
/// parentless device (HP's WMI hotkeys: `BUS_HOST`, a few keys and
/// switches) never is.
#[must_use]
pub const fn remote_client_input_candidate(device: InputDeviceFact, arcen_device: bool) -> bool {
    device.keyboard_or_pointer
        && device.console_seat
        && device.virtual_device
        && device.bus != BUS_HOST
        && device.bus != BUS_BLUETOOTH
        && !device.bridged
        && !arcen_device
}

/// Whether the device is a remote client's input of another product
/// ([`remote_client_input_candidate`]) while a process outside Arcen holds
/// the user-space input interfaces (`creators`): remote-desktop products
/// create their client's keyboard and pointer through them while that
/// client is connected, so on a desktop another product owns, such a
/// device means that product's client is using the desktop now.
#[must_use]
pub const fn another_products_client_input(
    device: InputDeviceFact,
    arcen_device: bool,
    creators: UserSpaceInputCreators,
) -> bool {
    remote_client_input_candidate(device, arcen_device) && creators.include_another_product()
}

/// The console desktops another remote-desktop product started (the
/// display manager and Arcen did not), other than the joined session
/// `joined`.
#[must_use]
pub fn other_product_desktops<'a>(
    sessions: &'a [ConsoleSessionFact],
    joined: &str,
) -> Vec<&'a str> {
    sessions
        .iter()
        .filter(|session| {
            session.id != joined
                && session.is_console_desktop()
                && session.origin() == ConsoleOrigin::OtherProduct
        })
        .map(|session| session.id.as_str())
        .collect()
}

/// Whether another remote-desktop product started a desktop of its own on
/// the console while the session `joined` was attached: one of
/// [`other_product_desktops`] that was not there when the attachment began
/// (`before`). ADR 0025 §8, "the other product is the display manager".
#[must_use]
pub fn other_product_started(
    sessions: &[ConsoleSessionFact],
    joined: &str,
    before: &[String],
) -> bool {
    other_product_desktops(sessions, joined)
        .into_iter()
        .any(|id| !before.iter().any(|known| known == id))
}

/// How a console session that did not lock may still be given back to the
/// desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockedHandBack {
    /// It is gone, or nothing of it can be shown any more.
    Gone,
    /// Another product's session that is not in the foreground: given back
    /// as it is (nothing of Arcen's can lock it).
    InBackground,
    /// In the foreground: put a login screen in front of it. A display
    /// manager's session then stays private behind it until it locks
    /// ([`Self::KeepPrivate`]): the login screen is one virtual-terminal
    /// switch away from it.
    BehindLoginScreen,
    /// Not given back: the desk stays private (monitors blank, its input
    /// disabled in the session's X server) and securing is retried.
    ///
    /// Arcen's own login session (a host without a display manager) is
    /// never given back unlocked: GNOME creates its lock screen only when
    /// GDM answers on the system bus, nothing else would cover the session,
    /// and a background virtual terminal is one key chord away. It stays
    /// private until the account signs in through Arcen again or signs out
    /// (its X server ends). Nor is a session logind could not be asked
    /// about: nothing is proven. Nor a display manager's session in the
    /// background (behind its login screen, or another session): a key
    /// chord at the desk switches to its virtual terminal, so it stays
    /// private there until it locks.
    KeepPrivate,
}

/// How long a display manager's session that will not lock is kept
/// private behind the login screen before it is given back there unlocked:
/// one whose lock screen is turned off by policy never locks, and its
/// owner, signing in at the login screen, would otherwise find it blank
/// and unusable for good. A lock that only failed for a moment (a busy
/// shell) is retried and taken well within it.
pub const UNLOCKED_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

impl UnlockedHandBack {
    /// This decision on a console whose login screen is `login_screen`: a
    /// session that would be put behind a login screen stays private
    /// instead where GDM may not be asked for one
    /// ([`LoginScreen::may_ask_for_login_screen`]): such a login screen
    /// never reaches the console and later takes the GPU from the product
    /// that runs it.
    #[must_use]
    pub const fn on(self, login_screen: LoginScreen) -> Self {
        match self {
            Self::BehindLoginScreen if !login_screen.may_ask_for_login_screen() => {
                Self::KeepPrivate
            }
            decision => decision,
        }
    }
}

/// What logind said about a console session.
#[derive(Debug, Clone, Copy)]
pub enum SessionLookup<'a> {
    /// logind describes it. `display_alive`: its X server still answers on
    /// its display (or that could not be ruled out).
    Known {
        fact: &'a ConsoleSessionFact,
        display_alive: bool,
    },
    /// logind no longer has it.
    Unknown,
    /// logind could not be asked, or did not answer in time.
    Failed,
}

/// Decides how a console session that did not lock may be given back to
/// the desk.
#[must_use]
pub fn unlocked_hand_back(lookup: SessionLookup<'_>) -> UnlockedHandBack {
    match lookup {
        SessionLookup::Failed => UnlockedHandBack::KeepPrivate,
        SessionLookup::Unknown => UnlockedHandBack::Gone,
        SessionLookup::Known {
            fact,
            display_alive,
        } if fact.origin() == ConsoleOrigin::Arcen => {
            // Closing is not gone: its X server may outlive a dead leader.
            if display_alive {
                UnlockedHandBack::KeepPrivate
            } else {
                UnlockedHandBack::Gone
            }
        }
        SessionLookup::Known { fact, .. } if fact.state == "closing" => UnlockedHandBack::Gone,
        SessionLookup::Known { fact, .. } if fact.active => UnlockedHandBack::BehindLoginScreen,
        SessionLookup::Known { fact, .. } if fact.origin() == ConsoleOrigin::DisplayManager => {
            UnlockedHandBack::KeepPrivate
        }
        SessionLookup::Known { .. } => UnlockedHandBack::InBackground,
    }
}

/// [`unlocked_hand_back`] for a session Arcen has failed to lock for
/// `unlocked_for`: after [`UNLOCKED_GRACE`] a display manager's session in
/// the background is given back there, as it was before it was kept
/// private (`InBackground`).
#[must_use]
pub fn unlocked_hand_back_after(
    lookup: SessionLookup<'_>,
    unlocked_for: std::time::Duration,
) -> UnlockedHandBack {
    let decision = unlocked_hand_back(lookup);
    match lookup {
        SessionLookup::Known { fact, .. }
            if decision == UnlockedHandBack::KeepPrivate
                && fact.origin() == ConsoleOrigin::DisplayManager
                && !fact.active
                && unlocked_for >= UNLOCKED_GRACE =>
        {
            UnlockedHandBack::InBackground
        }
        _ => decision,
    }
}

/// One step of handing the console back when a Deck leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachStep {
    /// Lock the session (it lives on until the user logs out).
    LockSession,
    /// Wait, bounded, until the lock screen is up before showing anything.
    AwaitLockScreen,
    /// Give the local monitors their own picture back.
    RestoreOutputs,
    /// Give the desk its sound back: the session's audio lease ends, so
    /// the default sink and every mute are what they were before the Deck
    /// attached (or what the user chose since), and Arcen's session sink
    /// goes.
    RestoreAudio,
    /// Give the local keyboard, mouse and tablets back.
    ReleaseInput,
}

/// The order a Deck's departure hands the console back in: lock first so
/// the monitors and the speakers come back on the lock screen, never on the
/// desktop; input last so nobody can act before the screen is locked.
/// Nothing ends the session.
pub const DETACH_STEPS: [DetachStep; 5] = [
    DetachStep::LockSession,
    DetachStep::AwaitLockScreen,
    DetachStep::RestoreOutputs,
    DetachStep::RestoreAudio,
    DetachStep::ReleaseInput,
];

/// The hand-back order on a host that lets only the session's own,
/// unlocked desktop change its displays and input hooks (Windows refuses
/// `SetDisplayConfig` from a process whose desktop is not the input
/// desktop, and locking makes the secure desktop the input desktop): the
/// outputs and the desk's input go back first, then the lock, and the sound
/// only once the screen is locked. Nothing ends the session.
pub const DETACH_STEPS_OUTPUTS_FIRST: [DetachStep; 5] = [
    DetachStep::RestoreOutputs,
    DetachStep::ReleaseInput,
    DetachStep::LockSession,
    DetachStep::AwaitLockScreen,
    DetachStep::RestoreAudio,
];

/// Why a Deck's attachment to the user's session ended for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Departure {
    /// The Deck disconnected, or did not ask to resume when its transport
    /// was lost.
    DeckLeft,
    /// The Deck's reconnect window ended without a resume, or a resume was
    /// refused.
    ReconnectWindowEnded,
    /// The Pier is stopping.
    HostStopping,
    /// The Pier's session agent failed.
    AgentFailed,
    /// A fresh sign-in of the same account took the session over.
    HandedOver,
    /// The session is no longer the one the Deck joined (signed out, locked
    /// at the desk, moved to another station or account).
    SessionChanged,
    /// A person stopped the host's own capture (macOS's Stop Sharing), so
    /// the attachment ended ([`stream_end::StreamEnd::CaptureStopped`]).
    CaptureStopped,
}

impl Departure {
    /// Whether the session is locked when the Deck leaves this way: always,
    /// except when its own account takes it over at once, or it is no
    /// longer the session the Deck joined.
    #[must_use]
    pub const fn locks_session(self) -> bool {
        !matches!(self, Self::HandedOver | Self::SessionChanged)
    }

    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::DeckLeft => "deck_left",
            Self::ReconnectWindowEnded => "reconnect_window_ended",
            Self::HostStopping => "host_stopping",
            Self::AgentFailed => "agent_failed",
            Self::HandedOver => "handed_over",
            Self::SessionChanged => "session_changed",
            Self::CaptureStopped => "capture_stopped",
        }
    }
}

/// Whether the console is still the session's account's when a departure
/// decides to lock it, from two readings: whether the console user reads as
/// the account, and whether the session's own record says it is on the
/// console (`None`: no record, the session has ended). Fails closed: a
/// console-user read that failed often looks like nobody (the login
/// screen), so the session is still ours while either reading says so. A
/// session that signed out (no record) or was switched away (not on the
/// console) is not, and is left alone ([`Departure::SessionChanged`]).
#[must_use]
pub const fn console_still_ours(account_holds_console: bool, on_console: Option<bool>) -> bool {
    account_holds_console || matches!(on_console, Some(true))
}

/// A local X display name (`:N` or `:N.S`) as its number, or `None` for a
/// remote or malformed one: a console session's display is always local.
#[must_use]
pub fn local_display_number(display: &str) -> Option<u32> {
    let rest = display.strip_prefix(':')?;
    let number = match rest.split_once('.') {
        None => rest,
        Some((number, screen))
            if !screen.is_empty() && screen.bytes().all(|b| b.is_ascii_digit()) =>
        {
            number
        }
        Some(_) => return None,
    };
    if number.is_empty() || number.len() > 4 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// Whether an X authority file may be handed to the user's capture and
/// input helpers: a regular file owned by the session's user (GDM and other
/// products' X servers both write one) or by root, writable by nobody else.
#[must_use]
pub const fn xauthority_trustworthy(
    regular: bool,
    owner: u32,
    mode: u32,
    session_uid: u32,
) -> bool {
    regular && (owner == session_uid || owner == 0) && mode & 0o022 == 0
}

/// Whether a session's cookie may be sent to the X server answering on its
/// socket: an X server run by root (the display manager's, another
/// product's), or the session user's own X server presenting the user's own
/// cookie. The socket directory is world-writable, so anyone may listen on
/// a display number nobody serves; a cookie from a root-owned file (or
/// anyone else's) never goes to such a listener.
#[must_use]
pub const fn x_server_peer_trustworthy(peer_uid: u32, cookie_owner: u32, session_uid: u32) -> bool {
    peer_uid == 0 || (peer_uid == session_uid && cookie_owner == session_uid)
}

/// The `MIT-MAGIC-COOKIE-1` an X authority file (`Xauthority` format: per
/// entry a big-endian `u16` family and four length-prefixed fields:
/// address, display number, name, data) holds for display `number`; an
/// entry for that exact number wins over one for any display.
#[must_use]
pub fn mit_magic_cookie(file: &[u8], number: u32) -> Option<Vec<u8>> {
    const NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";
    let wanted = number.to_string();
    let mut rest = file;
    let mut any_display = None;
    let field = |rest: &mut &[u8]| -> Option<Vec<u8>> {
        let (length, tail) = rest.split_first_chunk::<2>()?;
        let length = usize::from(u16::from_be_bytes(*length));
        if tail.len() < length {
            return None;
        }
        let (value, tail) = tail.split_at(length);
        *rest = tail;
        Some(value.to_vec())
    };
    while !rest.is_empty() {
        let (_family, tail) = rest.split_first_chunk::<2>()?;
        rest = tail;
        let _address = field(&mut rest)?;
        let display = field(&mut rest)?;
        let name = field(&mut rest)?;
        let data = field(&mut rest)?;
        if name != NAME || data.len() != 16 {
            continue;
        }
        if display == wanted.as_bytes() {
            return Some(data);
        }
        if display.is_empty() && any_display.is_none() {
            any_display = Some(data);
        }
    }
    any_display
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xauth_entry(
        family: u16,
        address: &[u8],
        display: &[u8],
        name: &[u8],
        data: &[u8],
    ) -> Vec<u8> {
        let mut entry = family.to_be_bytes().to_vec();
        for field in [address, display, name, data] {
            entry.extend_from_slice(&u16::try_from(field.len()).unwrap().to_be_bytes());
            entry.extend_from_slice(field);
        }
        entry
    }

    #[test]
    fn the_cookie_for_the_sessions_display_is_found() {
        let cookie = [7_u8; 16];
        let other = [9_u8; 16];
        let mut file = xauth_entry(256, b"host", b"0", b"MIT-MAGIC-COOKIE-1", &other);
        file.extend(xauth_entry(
            65535,
            b"",
            b"",
            b"MIT-MAGIC-COOKIE-1",
            &[1; 16],
        ));
        file.extend(xauth_entry(
            256,
            b"host",
            b"1",
            b"XDM-AUTHORIZATION-1",
            &[3; 16],
        ));
        file.extend(xauth_entry(
            256,
            b"host",
            b"1",
            b"MIT-MAGIC-COOKIE-1",
            &cookie,
        ));
        assert_eq!(mit_magic_cookie(&file, 1), Some(cookie.to_vec()));
        assert_eq!(
            mit_magic_cookie(&file, 5),
            Some(vec![1; 16]),
            "any-display entry"
        );
        assert_eq!(
            mit_magic_cookie(&file[..file.len() - 3], 1),
            None,
            "truncated"
        );
        assert_eq!(mit_magic_cookie(&[], 1), None);
    }

    fn session(id: &str, uid: u32, class: &str, kind: &str, active: bool) -> ConsoleSessionFact {
        ConsoleSessionFact {
            id: id.to_owned(),
            uid,
            user: format!("u{uid}"),
            seat: CONSOLE_SEAT.to_owned(),
            vtnr: 2,
            tty: String::new(),
            class: class.to_owned(),
            session_type: kind.to_owned(),
            state: if active { "active" } else { "online" }.to_owned(),
            active,
            remote: false,
            service: "gdm-password".to_owned(),
            locked: false,
            leader: 0,
            desktop: String::new(),
        }
    }

    fn greeter(active: bool) -> ConsoleSessionFact {
        let mut greeter = session("c1", 42, "greeter", "x11", active);
        greeter.service = "gdm-launch-environment".to_owned();
        greeter.vtnr = 1;
        greeter
    }

    const TIM: u32 = 1001;
    const OTHER: u32 = 1002;

    #[test]
    fn the_users_own_active_desktop_is_joined_as_it_is() {
        let sessions = [greeter(false), session("7", TIM, "user", "x11", true)];
        assert_eq!(
            decide(&sessions, TIM),
            ConsoleDecision::Attach {
                session: "7".into(),
                activate: false,
                origin: ConsoleOrigin::DisplayManager
            }
        );
    }

    #[test]
    fn a_session_waiting_behind_the_login_screen_is_brought_forward() {
        let sessions = [greeter(true), session("7", TIM, "user", "x11", false)];
        assert_eq!(
            decide(&sessions, TIM),
            ConsoleDecision::Attach {
                session: "7".into(),
                activate: true,
                origin: ConsoleOrigin::DisplayManager
            }
        );
    }

    fn other_product(id: &str, uid: u32, active: bool) -> ConsoleSessionFact {
        let mut other = session(id, uid, "user", "x11", active);
        other.service = "vendor-session".into();
        other.vtnr = 7;
        // Whatever remote host the product gave PAM.
        other.remote = true;
        other
    }

    #[test]
    fn another_account_at_the_console_is_refused_and_another_product_is_named() {
        let sessions = [session("7", OTHER, "user", "x11", true)];
        assert_eq!(
            decide(&sessions, TIM),
            ConsoleDecision::Refuse(ConsoleRefusal::OtherUser)
        );
        assert_eq!(
            decide(&[greeter(true), other_product("9", OTHER, false)], TIM),
            ConsoleDecision::Refuse(ConsoleRefusal::OtherUserOtherProduct),
            "another account's session outside the display manager keeps the console in the background"
        );
        assert_eq!(
            decide(&[other_product("9", OTHER, true)], TIM),
            ConsoleDecision::Refuse(ConsoleRefusal::OtherUserOtherProduct)
        );
    }

    #[test]
    fn a_returning_deck_rejoins_only_its_own_session_and_is_told_why_not() {
        let held = session("7", TIM, "user", "x11", true);
        assert_eq!(
            decide_reattach(&[held.clone()], TIM, "7", ConsoleHost::default()),
            Ok(false)
        );
        assert_eq!(
            decide_reattach(
                &[greeter(true), session("7", TIM, "user", "x11", false)],
                TIM,
                "7",
                ConsoleHost::default()
            ),
            Ok(true)
        );
        assert_eq!(
            decide_reattach(
                &[
                    session("7", TIM, "user", "x11", false),
                    other_product("9", OTHER, true)
                ],
                TIM,
                "7",
                ConsoleHost::default()
            ),
            Err(ConsoleRefusal::OtherUserOtherProduct)
        );
        assert_eq!(
            decide_reattach(
                &[session("8", OTHER, "user", "x11", true)],
                TIM,
                "7",
                ConsoleHost::default()
            ),
            Err(ConsoleRefusal::OtherUser)
        );
        assert_eq!(
            decide_reattach(
                &[session("12", TIM, "user", "x11", true)],
                TIM,
                "7",
                ConsoleHost::default()
            ),
            Err(ConsoleRefusal::SessionChanged),
            "signed out and in again at the desk"
        );
        assert_eq!(
            decide_reattach(&[greeter(true)], TIM, "7", ConsoleHost::default()),
            Err(ConsoleRefusal::SessionChanged),
            "signed out"
        );
        let mut foreign = held;
        foreign.uid = OTHER;
        assert_eq!(
            decide_reattach(
                &[foreign, session("7b", TIM, "user", "x11", false)],
                TIM,
                "7",
                ConsoleHost::default()
            ),
            Err(ConsoleRefusal::OtherUser)
        );
    }

    fn arcen_login(id: &str, uid: u32, vt: u32) -> ConsoleSessionFact {
        let mut session = session(id, uid, "user", "x11", true);
        session.service = login_session::PAM_SERVICE.into();
        session.vtnr = vt;
        session.tty = format!("tty{vt}");
        session
    }

    #[test]
    fn a_stopped_product_aliased_as_the_display_manager_is_no_display_manager() {
        let host = |target, active| ConsoleHost::from_display_manager(target, active);
        assert!(!host(None, false).display_manager);
        assert!(host(Some("gdm.service"), false).display_manager);
        assert!(host(Some("gdm.service"), true).display_manager);
        assert!(host(Some("remote-product.service"), true).display_manager);
        assert!(!host(Some("remote-product.service"), false).display_manager);
        assert_eq!(
            decide_on(&[], TIM, host(Some("remote-product.service"), false)),
            ConsoleDecision::StartLoginSession { vt: 7 }
        );
    }

    #[test]
    fn a_product_that_runs_the_sign_in_is_never_bypassed_through_a_login_screen() {
        let gdm =
            ConsoleHost::from_display_manager_and_gdm(Some("gdm.service"), true, || Some(false));
        let withheld =
            ConsoleHost::from_display_manager_and_gdm(Some("labproduct.service"), true, || {
                Some(false)
            });
        let shown =
            ConsoleHost::from_display_manager_and_gdm(Some("labproduct.service"), true, || {
                Some(true)
            });
        let unproven =
            ConsoleHost::from_display_manager_and_gdm(Some("labproduct.service"), true, || None);
        let stopped =
            ConsoleHost::from_display_manager_and_gdm(Some("labproduct.service"), false, || {
                Some(false)
            });
        assert_eq!(gdm, ConsoleHost::default(), "GDM as the display manager");
        assert_eq!(withheld.login_screen, LoginScreen::Withheld);
        assert!(withheld.display_manager);
        assert_eq!(stopped.login_screen, LoginScreen::Shown);
        assert!(!stopped.display_manager);
        let mut product_desktop = session("26", TIM, "user", "x11", true);
        product_desktop.service = "labproduct-session".into();
        let mut others = product_desktop.clone();
        others.uid = OTHER;
        let sign_in = ConsoleDecision::SignInThroughGreeter {
            greeter: "c1".into(),
        };
        let refused = ConsoleDecision::Refuse(ConsoleRefusal::SignInOtherProduct);
        let rows: Vec<(&str, Vec<ConsoleSessionFact>, ConsoleHost, ConsoleDecision)> = vec![
            // GDM shows its login screen: unchanged.
            ("gdm, greeter", vec![greeter(true)], gdm, sign_in.clone()),
            (
                "gdm, nothing",
                vec![],
                gdm,
                ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession),
            ),
            (
                "product shows it",
                vec![greeter(true)],
                shown,
                sign_in.clone(),
            ),
            ("product, unproven", vec![greeter(true)], unproven, sign_in),
            (
                "product unproven, nothing",
                vec![],
                unproven,
                ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession),
            ),
            (
                "product stopped",
                vec![],
                stopped,
                ConsoleDecision::StartLoginSession { vt: 7 },
            ),
            // The product runs the sign-in: refused, a leftover login
            // screen (even a Wayland one) included.
            ("withheld, nothing", vec![], withheld, refused.clone()),
            (
                "withheld, leftover",
                vec![greeter(true)],
                withheld,
                refused.clone(),
            ),
            (
                "withheld, leftover Wayland",
                vec![{
                    let mut wayland = greeter(false);
                    wayland.session_type = "wayland".into();
                    wayland
                }],
                withheld,
                refused,
            ),
            // The account's desktop in the product is joined as before.
            (
                "withheld, own desktop",
                vec![greeter(false), product_desktop],
                withheld,
                ConsoleDecision::Attach {
                    session: "26".into(),
                    activate: false,
                    origin: ConsoleOrigin::OtherProduct,
                },
            ),
            (
                "withheld, another account",
                vec![others],
                withheld,
                ConsoleDecision::Refuse(ConsoleRefusal::OtherUserOtherProduct),
            ),
        ];
        for (row, sessions, host, expected) in rows {
            assert_eq!(decide_on(&sessions, TIM, host), expected, "{row}");
        }
    }

    #[test]
    fn a_session_that_did_not_lock_is_kept_private_where_gdm_may_not_be_asked() {
        let front = session("26", TIM, "user", "x11", true);
        let behind = unlocked_hand_back(SessionLookup::Known {
            fact: &front,
            display_alive: true,
        });
        assert_eq!(behind, UnlockedHandBack::BehindLoginScreen);
        assert_eq!(behind.on(LoginScreen::Shown), behind, "GDM's own host");
        assert_eq!(
            behind.on(LoginScreen::Withheld),
            UnlockedHandBack::KeepPrivate
        );
        assert_eq!(
            behind.on(LoginScreen::Unproven),
            UnlockedHandBack::KeepPrivate
        );
        for other in [
            UnlockedHandBack::Gone,
            UnlockedHandBack::InBackground,
            UnlockedHandBack::KeepPrivate,
        ] {
            assert_eq!(other.on(LoginScreen::Withheld), other);
        }
    }

    #[test]
    fn a_host_without_a_display_manager_gets_a_real_login_session_once_per_account() {
        let no_dm = ConsoleHost {
            display_manager: false,
            login_screen: LoginScreen::Shown,
        };
        assert_eq!(
            decide_on(&[], TIM, no_dm),
            ConsoleDecision::StartLoginSession { vt: 7 }
        );
        // A text login on VT 7 pushes it along; a display manager's host refuses.
        let mut getty = session("c2", TIM, "user", "tty", true);
        getty.vtnr = 7;
        assert_eq!(
            decide_on(&[getty], TIM, no_dm),
            ConsoleDecision::StartLoginSession { vt: 8 }
        );
        assert_eq!(
            decide(&[], TIM),
            ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession)
        );
        // The account's own session is joined, as any console session.
        assert_eq!(
            decide_on(&[arcen_login("40", TIM, 7)], TIM, no_dm),
            ConsoleDecision::Attach {
                session: "40".into(),
                activate: false,
                origin: ConsoleOrigin::Arcen
            }
        );
        // Another account's is refused, even in the background.
        let mut background = arcen_login("41", OTHER, 7);
        background.active = false;
        background.state = "online".into();
        for sessions in [vec![arcen_login("41", OTHER, 7)], vec![background]] {
            assert_eq!(
                decide_on(&sessions, TIM, no_dm),
                ConsoleDecision::Refuse(ConsoleRefusal::OtherUser)
            );
        }
        // A returning Deck whose session ended is not given a new one.
        assert_eq!(
            decide_reattach(&[], TIM, "40", no_dm),
            Err(ConsoleRefusal::SessionChanged)
        );
    }

    #[test]
    fn the_holder_of_another_products_refusal_is_found() {
        let sessions = [
            greeter(true),
            other_product("9", OTHER, false),
            other_product("10", OTHER, true),
        ];
        assert_eq!(
            other_product_holder(&sessions, TIM).map(|s| s.id.as_str()),
            Some("10")
        );
        assert_eq!(
            other_product_holder(&sessions[..2], TIM).map(|s| s.id.as_str()),
            Some("9")
        );
        assert_eq!(
            other_product_holder(&[other_product("9", TIM, true)], TIM),
            None
        );
    }

    #[test]
    fn the_users_own_session_of_another_product_is_joined() {
        assert_eq!(
            decide(&[other_product("9", TIM, true)], TIM),
            ConsoleDecision::Attach {
                session: "9".into(),
                activate: false,
                origin: ConsoleOrigin::OtherProduct
            }
        );
    }

    #[test]
    fn with_only_the_login_screen_the_sign_in_goes_through_it() {
        assert_eq!(
            decide(&[greeter(true)], TIM),
            ConsoleDecision::SignInThroughGreeter {
                greeter: "c1".into()
            }
        );
        // A long-idle greeter logind no longer reports active (lab, GDM 40).
        assert_eq!(
            decide(&[greeter(false)], TIM),
            ConsoleDecision::SignInThroughGreeter {
                greeter: "c1".into()
            }
        );
        // Another account switched away (fast user switching): its session
        // waits in the background and the login screen is free to use.
        assert_eq!(
            decide(
                &[greeter(true), session("8", OTHER, "user", "x11", false)],
                TIM
            ),
            ConsoleDecision::SignInThroughGreeter {
                greeter: "c1".into()
            }
        );
    }

    #[test]
    fn wayland_desktops_are_targets_and_wayland_login_screens_are_refused_visibly() {
        assert!(matches!(
            decide(&[session("7", TIM, "user", "wayland", true)], TIM),
            ConsoleDecision::JoinWayland(target) if target.session == "7" && !target.activate
        ));
        assert_eq!(
            decide_reattach(
                &[session("7", TIM, "user", "wayland", true)],
                TIM,
                "7",
                ConsoleHost::default()
            ),
            Err(ConsoleRefusal::WaylandSession),
            "a held attachment is never a Wayland one until the adapter exists"
        );
        let mut wayland_greeter = greeter(true);
        wayland_greeter.session_type = "wayland".into();
        assert_eq!(
            decide(&[wayland_greeter], TIM),
            ConsoleDecision::Refuse(ConsoleRefusal::WaylandSession)
        );
        // An X11 session of the user is preferred over a Wayland one.
        assert!(matches!(
            decide(
                &[
                    session("6", TIM, "user", "wayland", false),
                    session("7", TIM, "user", "x11", false)
                ],
                TIM
            ),
            ConsoleDecision::Attach { session, .. } if session == "7"
        ));
    }

    #[test]
    fn an_arcen_dedicated_desktop_is_not_a_console_session() {
        let mut dedicated = session("81142", TIM, "user", "x11", true);
        dedicated.remote = true;
        dedicated.tty = ARCEN_TTY.into();
        dedicated.service = "login".into();
        dedicated.vtnr = 1;
        assert_eq!(
            decide(&[greeter(false), dedicated], TIM),
            ConsoleDecision::SignInThroughGreeter {
                greeter: "c1".into()
            }
        );
    }

    #[test]
    fn nothing_on_the_console_is_refused_not_given_a_private_desktop() {
        let mut ssh = session("3", TIM, "user", "tty", true);
        ssh.seat = String::new();
        ssh.remote = true;
        assert_eq!(
            decide(&[ssh], TIM),
            ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession)
        );
        let mut closing = session("7", TIM, "user", "x11", false);
        closing.state = "closing".into();
        assert_eq!(
            decide(&[closing], TIM),
            ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession)
        );
    }

    #[test]
    fn loginctl_properties_parse_and_an_unnamed_or_ownerless_session_is_no_fact() {
        let fact = ConsoleSessionFact::from_properties(
            "Id=23025\nUser=2311\nName=lca\nSeat=seat0\nVTNr=7\nClass=user\nType=x11\n\
             State=active\nActive=yes\nRemote=no\nService=gdm-ovirtcred\nLockedHint=yes\nDisplay=\nLeader=4242\nDesktop=gnome\n",
        )
        .expect("fact");
        assert_eq!(fact.uid, 2311);
        assert_eq!(fact.leader, 4242);
        assert_eq!(fact.desktop, "gnome");
        assert!(fact.active && fact.locked && fact.is_console_desktop());
        assert_eq!(fact.origin(), ConsoleOrigin::DisplayManager);
        assert_eq!(ConsoleSessionFact::from_properties("User=1\n"), None);
        assert_eq!(ConsoleSessionFact::from_properties("Id=4\n"), None);
    }

    #[test]
    fn every_refusal_has_a_distinct_code_that_leads_a_short_deck_reason() {
        let mut codes = std::collections::BTreeSet::new();
        for refusal in ConsoleRefusal::ALL {
            assert!(codes.insert(refusal.token()));
            assert!(refusal.deck_reason().len() <= 120, "{}", refusal.token());
            assert!(refusal.deck_reason().starts_with(refusal.token()));
        }
        assert_eq!(
            ConsoleRefusal::other_user(ConsoleOrigin::OtherProduct),
            ConsoleRefusal::OtherUserOtherProduct
        );
    }

    #[test]
    fn another_products_desktop_beside_the_joined_one_is_noticed() {
        let mine = arcen_login("40", TIM, 7);
        assert!(!other_product_started(
            std::slice::from_ref(&mine),
            "40",
            &[]
        ));
        let mut product = session("81", TIM, "user", "x11", true);
        product.service = "remote-product-session".into();
        let both = [mine, product.clone()];
        assert_eq!(other_product_desktops(&both, "40"), ["81"]);
        assert!(other_product_started(&both, "40", &[]));
        // One that was already there when the attachment began is not news.
        assert!(!other_product_started(&both, "40", &["81".to_owned()]));
        // The joined session itself is never "another".
        assert!(!other_product_started(&[product], "81", &[]));
    }

    #[test]
    fn a_cookie_goes_only_to_root_x_servers_or_the_users_own_with_its_own_cookie() {
        assert!(x_server_peer_trustworthy(0, 0, TIM));
        assert!(x_server_peer_trustworthy(0, TIM, TIM));
        assert!(x_server_peer_trustworthy(TIM, TIM, TIM));
        // A listener of the user's on a display nobody serves never gets a
        // root-owned cookie, nor anyone else's.
        assert!(!x_server_peer_trustworthy(TIM, 0, TIM));
        assert!(!x_server_peer_trustworthy(TIM, OTHER, TIM));
        assert!(!x_server_peer_trustworthy(OTHER, OTHER, TIM));
    }

    /// A uinput keyboard of another product's connected client, shaped like
    /// the field's (no parent, bus 0, vendor 0, udev keyboard).
    const PRODUCT_KEYBOARD: InputDeviceFact = InputDeviceFact {
        virtual_device: true,
        console_seat: true,
        bridged: false,
        drives_desktop: true,
        keyboard_or_pointer: true,
        bus: 0,
    };

    /// A root service running a packaged file (`-rwxr-xr-x root root`).
    const ROOT_SERVICE: LocalDriverFacts = LocalDriverFacts {
        runs_as_root: true,
        executable: Some(ExecutableOwner {
            uid: 0,
            gid: 0,
            mode: 0o100_755,
        }),
    };

    #[test]
    fn a_driver_at_the_desk_is_not_a_remote_product() {
        for driver in [
            "/opt/Tangent/bin/TangentHub",
            "/opt/Tangent/bin/TangentHub (deleted)",
            "/usr/libexec/bluetooth/bluetoothd",
            "/usr/lib/bluetooth/bluetoothd",
            "/usr/bin/logid",
            "/usr/local/bin/xremap",
        ] {
            assert!(local_input_driver(driver, ROOT_SERVICE), "{driver}");
        }
        // Tools another program can type through are never drivers at the
        // desk, however they are installed.
        for tool in [
            "/usr/lib/remote-product/server",
            "/opt/evil/TangentHub-fake",
            "/usr/bin/ydotoold",
            "/usr/bin/dotoold",
            "/usr/bin/keyd",
            "/usr/bin/kanata",
            "/usr/bin/uinput",
            "",
        ] {
            assert!(!local_input_driver(tool, ROOT_SERVICE), "{tool}");
        }
    }

    #[test]
    fn a_drivers_name_alone_never_makes_one() {
        const PANEL: &str = "/opt/Tangent/bin/TangentHub";
        let owned = |uid, gid, mode| LocalDriverFacts {
            executable: Some(ExecutableOwner { uid, gid, mode }),
            ..ROOT_SERVICE
        };
        // Another product's program renamed by the session's user: their
        // own file, or run by them.
        for facts in [
            owned(1000, 1000, 0o100_755),
            LocalDriverFacts {
                runs_as_root: false,
                ..ROOT_SERVICE
            },
            LocalDriverFacts {
                executable: None,
                ..ROOT_SERVICE
            },
            // Root's file that anyone, or a group other than root's, may
            // rewrite.
            owned(0, 0, 0o100_777),
            owned(0, 100, 0o100_775),
            // Not a regular file.
            owned(0, 0, 0o040_755),
        ] {
            assert!(!local_input_driver(PANEL, facts), "{facts:?}");
        }
        // Writable by root's own group only is still root's alone.
        assert!(local_input_driver(PANEL, owned(0, 0, 0o100_775)));
        // The field's panel service file is set-user-ID root (4755): still
        // root's alone. A user who runs it keeps their real user ID, so the
        // process is not root's (`runs_as_root` false, above).
        assert!(local_input_driver(PANEL, owned(0, 0, 0o104_755)));
    }

    #[test]
    fn a_device_is_attributed_to_the_interface_that_made_it() {
        use UserSpaceInputInterface::{Uhid, Uinput};
        assert_eq!(
            UserSpaceInputInterface::of_device("/sys/devices/virtual/input/input213"),
            Some(Uinput)
        );
        assert_eq!(
            UserSpaceInputInterface::of_device(
                "/sys/devices/virtual/misc/uhid/0005:046D:B35B.0004/input/input21"
            ),
            Some(Uhid)
        );
        assert_eq!(
            UserSpaceInputInterface::of_device("/sys/devices/virtual/other/input/input9"),
            None
        );
        let uhid_only = HeldInterfaces {
            uinput: false,
            uhid: true,
        };
        // bluetoothd holding uhid for the desk's Bluetooth keyboard cannot
        // have made a uinput device.
        assert!(!uhid_only.could_make(Some(Uinput)));
        assert!(uhid_only.could_make(Some(Uhid)));
        assert!(uhid_only.could_make(None));
        let uinput_only = HeldInterfaces {
            uinput: true,
            uhid: false,
        };
        assert!(uinput_only.could_make(Some(Uinput)));
        assert!(!uinput_only.could_make(Some(Uhid)));
        assert!(!HeldInterfaces::default().could_make(None));
    }

    #[test]
    fn another_products_client_shows_as_its_software_input() {
        use UserSpaceInputCreators::{ArcenOnly, Other, Unknown};
        assert!(another_products_client_input(
            PRODUCT_KEYBOARD,
            false,
            Other
        ));
        // One made over the virtual bus or as a USB device is the same.
        for bus in [0x03, 0x06] {
            let device = InputDeviceFact {
                bus,
                ..PRODUCT_KEYBOARD
            };
            assert!(another_products_client_input(device, false, Other));
        }
        // A creator that could not be ruled out still counts.
        assert!(another_products_client_input(
            PRODUCT_KEYBOARD,
            false,
            Unknown
        ));
        // With nothing outside Arcen holding uinput or uhid, no device made
        // in user space can be another product's.
        assert!(!another_products_client_input(
            PRODUCT_KEYBOARD,
            false,
            ArcenOnly
        ));
        // Arcen's own virtual devices never count.
        assert!(!another_products_client_input(
            PRODUCT_KEYBOARD,
            true,
            Other
        ));
        for device in [
            InputDeviceFact {
                virtual_device: false,
                ..PRODUCT_KEYBOARD
            },
            InputDeviceFact {
                bridged: true,
                ..PRODUCT_KEYBOARD
            },
            InputDeviceFact {
                drives_desktop: false,
                keyboard_or_pointer: false,
                ..PRODUCT_KEYBOARD
            },
            InputDeviceFact {
                console_seat: false,
                ..PRODUCT_KEYBOARD
            },
        ] {
            assert!(
                !another_products_client_input(device, false, Other),
                "{device:?}"
            );
        }
    }

    #[test]
    fn a_firmware_hotkey_device_is_never_another_products_client() {
        use UserSpaceInputCreators::{Other, Unknown};
        // The field workstation's `HP WMI hotkeys`: registered by the hp-wmi
        // driver without a parent (so under /sys/devices/virtual), on
        // BUS_HOST, udev ID_INPUT_KEY and ID_INPUT_SWITCH only. Present on
        // every boot, with no remote client anywhere.
        let hotkeys = InputDeviceFact {
            virtual_device: true,
            console_seat: true,
            bridged: false,
            drives_desktop: true,
            keyboard_or_pointer: false,
            bus: BUS_HOST,
        };
        for creators in [Other, Unknown] {
            assert!(!another_products_client_input(hotkeys, false, creators));
        }
        // Either fact alone rules it out: a platform device with a full
        // keyboard's keys, or a few hotkeys on any other bus.
        let platform_keyboard = InputDeviceFact {
            keyboard_or_pointer: true,
            ..hotkeys
        };
        assert!(!remote_client_input_candidate(platform_keyboard, false));
        let few_keys = InputDeviceFact { bus: 0, ..hotkeys };
        assert!(!remote_client_input_candidate(few_keys, false));
        // Without a parent it is left alone while attached, as before.
        assert!(!disables_while_attached(hotkeys));
    }

    #[test]
    fn a_bluetooth_keyboard_at_the_desk_is_never_another_products_client() {
        // BlueZ makes a Bluetooth LE keyboard through uhid: no parent, and
        // bluetoothd (not Arcen) holds /dev/uhid.
        let keyboard = InputDeviceFact {
            bus: BUS_BLUETOOTH,
            ..PRODUCT_KEYBOARD
        };
        assert!(!another_products_client_input(
            keyboard,
            false,
            UserSpaceInputCreators::Other
        ));
    }

    #[test]
    fn only_the_desks_own_input_is_disabled() {
        let physical = InputDeviceFact {
            virtual_device: false,
            console_seat: true,
            bridged: false,
            drives_desktop: true,
            keyboard_or_pointer: true,
            bus: 0x03,
        };
        assert!(disables_while_attached(physical));
        for other in [
            InputDeviceFact {
                virtual_device: true,
                ..physical
            },
            InputDeviceFact {
                console_seat: false,
                ..physical
            },
            InputDeviceFact {
                bridged: true,
                ..physical
            },
            InputDeviceFact {
                drives_desktop: false,
                ..physical
            },
        ] {
            assert!(!disables_while_attached(other), "{other:?}");
        }
    }

    #[test]
    fn the_console_is_handed_back_locked_before_it_is_shown_or_usable() {
        let position = |step| DETACH_STEPS.iter().position(|s| *s == step).unwrap();
        assert!(position(DetachStep::LockSession) < position(DetachStep::AwaitLockScreen));
        assert!(position(DetachStep::AwaitLockScreen) < position(DetachStep::RestoreOutputs));
        assert!(position(DetachStep::RestoreOutputs) < position(DetachStep::ReleaseInput));
        // The desk's sound comes back with its picture, after the lock.
        assert!(position(DetachStep::AwaitLockScreen) < position(DetachStep::RestoreAudio));
        assert!(position(DetachStep::RestoreAudio) < position(DetachStep::ReleaseInput));
    }

    #[test]
    fn the_outputs_first_order_still_locks_before_the_sound_comes_back() {
        let position = |step| {
            DETACH_STEPS_OUTPUTS_FIRST
                .iter()
                .position(|s| *s == step)
                .unwrap()
        };
        assert!(position(DetachStep::RestoreOutputs) < position(DetachStep::LockSession));
        assert!(position(DetachStep::LockSession) < position(DetachStep::AwaitLockScreen));
        assert!(position(DetachStep::AwaitLockScreen) < position(DetachStep::RestoreAudio));
        for step in DETACH_STEPS {
            assert!(DETACH_STEPS_OUTPUTS_FIRST.contains(&step), "{step:?}");
        }
    }

    #[test]
    fn a_failed_console_read_never_skips_the_lock_of_a_session_on_screen() {
        // A failed console-user read looks like the login screen; the
        // session's own record still says it is on screen.
        assert!(console_still_ours(false, Some(true)));
        assert!(console_still_ours(true, None));
        assert!(console_still_ours(true, Some(false)));
        // Signed out (no record) or switched away (not on the console).
        assert!(!console_still_ours(false, None));
        assert!(!console_still_ours(false, Some(false)));
    }

    #[test]
    fn only_a_successor_or_a_changed_session_leaves_it_unlocked() {
        for departure in [
            Departure::DeckLeft,
            Departure::ReconnectWindowEnded,
            Departure::HostStopping,
            Departure::AgentFailed,
            Departure::CaptureStopped,
        ] {
            assert!(departure.locks_session(), "{}", departure.token());
        }
        assert!(!Departure::HandedOver.locks_session());
        assert!(!Departure::SessionChanged.locks_session());
    }

    #[test]
    fn only_local_display_names_are_accepted() {
        assert_eq!(local_display_number(":1"), Some(1));
        assert_eq!(local_display_number(":100.0"), Some(100));
        for bad in [
            "",
            "1",
            ":",
            "host:0",
            ":1.",
            ":1.x",
            ":-1",
            ":12345",
            "localhost:10.0",
        ] {
            assert_eq!(local_display_number(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_xauthority_file_must_belong_to_the_user_or_root_and_be_unwritable_by_others() {
        assert!(xauthority_trustworthy(true, TIM, 0o600, TIM));
        assert!(xauthority_trustworthy(true, 0, 0o644, TIM));
        assert!(!xauthority_trustworthy(true, OTHER, 0o600, TIM));
        assert!(!xauthority_trustworthy(true, TIM, 0o620, TIM));
        assert!(!xauthority_trustworthy(false, TIM, 0o600, TIM));
    }

    #[test]
    fn the_mode_defaults_to_joining_the_console() {
        assert_eq!(DesktopMode::default(), DesktopMode::Console);
        for mode in DesktopMode::ALL {
            assert_eq!(DesktopMode::from_token(mode.token()), Some(mode));
        }
        assert_eq!(DesktopMode::from_token("private"), None);
    }

    #[test]
    fn a_sign_in_settles_only_once_its_login_screen_is_gone() {
        let mine = session("7", TIM, "user", "x11", true);
        assert_eq!(
            settle_sign_in(&[greeter(true)], TIM, "c1"),
            SignInSettlement::Pending
        );
        assert_eq!(
            settle_sign_in(&[greeter(false), mine.clone()], TIM, "c1"),
            SignInSettlement::Pending,
            "GDM has not finished its login screen yet"
        );
        let mut lingering = greeter(false);
        lingering.state = "closing".into();
        assert_eq!(
            settle_sign_in(&[lingering, mine.clone()], TIM, "c1"),
            SignInSettlement::Settled("7".into()),
            "a closing login screen is finished; leftovers keep only its session"
        );
        assert_eq!(
            settle_sign_in(&[mine], TIM, "c1"),
            SignInSettlement::Settled("7".into())
        );
    }

    #[test]
    fn a_login_screen_respawned_in_front_of_the_new_session_is_put_behind_it() {
        let mut respawned = greeter(true);
        respawned.id = "c2".into();
        let mine = session("7", TIM, "user", "x11", false);
        assert_eq!(
            settle_sign_in(&[respawned.clone(), mine.clone()], TIM, "c1"),
            SignInSettlement::BringForward("7".into())
        );
        assert_eq!(
            settle_sign_in(&[mine.clone()], TIM, "c1"),
            SignInSettlement::Pending,
            "nothing in front yet: the VT switch is still under way"
        );
        let other = session("8", OTHER, "user", "x11", true);
        assert_eq!(
            settle_sign_in(&[respawned, other, mine], TIM, "c1"),
            SignInSettlement::Pending,
            "another account in front is never switched away"
        );
    }

    #[test]
    fn an_unlocked_arcen_login_session_is_never_given_back_to_the_desk() {
        fn known(fact: &ConsoleSessionFact, display_alive: bool) -> SessionLookup<'_> {
            SessionLookup::Known {
                fact,
                display_alive,
            }
        }
        let mut mine = session("7", TIM, "user", "x11", false);
        mine.service = login_session::PAM_SERVICE.into();
        assert_eq!(
            unlocked_hand_back(known(&mine, true)),
            UnlockedHandBack::KeepPrivate,
            "even in the background"
        );
        mine.active = true;
        assert_eq!(
            unlocked_hand_back(known(&mine, true)),
            UnlockedHandBack::KeepPrivate
        );
        mine.state = "closing".into();
        assert_eq!(
            unlocked_hand_back(known(&mine, true)),
            UnlockedHandBack::KeepPrivate,
            "closing while its X server still runs"
        );
        assert_eq!(
            unlocked_hand_back(known(&mine, false)),
            UnlockedHandBack::Gone,
            "signed out: its X server ended"
        );
        assert_eq!(
            unlocked_hand_back(SessionLookup::Unknown),
            UnlockedHandBack::Gone
        );
        assert_eq!(
            unlocked_hand_back(SessionLookup::Failed),
            UnlockedHandBack::KeepPrivate,
            "logind did not answer: nothing is proven"
        );
        let gdm = session("8", TIM, "user", "x11", false);
        assert_eq!(
            unlocked_hand_back(known(&gdm, true)),
            UnlockedHandBack::KeepPrivate,
            "a display manager's unlocked session is one key chord away behind the login screen"
        );
        let mut other = session("9", OTHER, "user", "x11", false);
        other.service = "labrdp-session".into();
        assert_eq!(
            unlocked_hand_back(known(&other, true)),
            UnlockedHandBack::InBackground
        );
        let gdm = session("8", TIM, "user", "x11", true);
        assert_eq!(
            unlocked_hand_back(known(&gdm, true)),
            UnlockedHandBack::BehindLoginScreen
        );
    }

    #[test]
    fn a_display_manager_session_that_never_locks_is_given_back_after_the_grace() {
        use std::time::Duration;
        let known = |fact| SessionLookup::Known {
            fact,
            display_alive: true,
        };
        let background = session("8", TIM, "user", "x11", false);
        assert_eq!(
            unlocked_hand_back_after(known(&background), Duration::from_secs(10)),
            UnlockedHandBack::KeepPrivate
        );
        assert_eq!(
            unlocked_hand_back_after(known(&background), UNLOCKED_GRACE),
            UnlockedHandBack::InBackground,
            "a lock screen turned off by policy: its owner gets it back at the login screen"
        );
        let front = session("8", TIM, "user", "x11", true);
        assert_eq!(
            unlocked_hand_back_after(known(&front), UNLOCKED_GRACE),
            UnlockedHandBack::BehindLoginScreen
        );
        let mut own = session("7", TIM, "user", "x11", false);
        own.service = login_session::PAM_SERVICE.into();
        assert_eq!(
            unlocked_hand_back_after(known(&own), UNLOCKED_GRACE * 100),
            UnlockedHandBack::KeepPrivate,
            "Arcen's own login session has no lock screen at all and stays private"
        );
        assert_eq!(
            unlocked_hand_back_after(SessionLookup::Failed, UNLOCKED_GRACE * 100),
            UnlockedHandBack::KeepPrivate
        );
    }
}
