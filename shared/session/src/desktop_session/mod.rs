//! The parts of a GNOME login that a headless Arcen desktop assembles
//! itself, as pure policy.
//!
//! A GDM login runs `gnome-session`, which starts the settings daemons and
//! the XDG autostart programs, serves `org.gnome.SessionManager` (presence,
//! idle, inhibitors, logout) and leaves the keyring unlocked. Since GNOME 49
//! that session can only run under the user's systemd manager, outside the
//! logind scope, private bus and private runtime directory an Arcen desktop
//! owns. The Linux Pier therefore assembles the same session inside its own
//! stack; this module holds every decision it takes, and nothing here
//! spawns, reads a file or talks D-Bus:
//!
//! - [`autostart`]: the freedesktop autostart and Desktop Entry rules plus
//!   GNOME's extensions (phases, conditions, auto-restart).
//! - [`components`]: which autostart entries an Arcen session runs, which it
//!   provides itself and which it never runs because they act on the host
//!   rather than the remote session.
//! - [`restart`]: the auto-restart budget.
//! - [`manager`]: the session manager's state (inhibitors, clients,
//!   presence, logout) and the policy constants where Arcen deliberately
//!   differs from `gnome-session`.
//! - [`lock`]: whether the screen may lock (only with an in-session unlock
//!   path).
//! - [`settings_overlay`]: the few settings a session pins, rendered for a
//!   per-session dconf database.
//! - [`login_environment`]: which variables a user's login shell may add.
//! - [`first_login`]: the operator's first-login policy and GNOME's own
//!   completion markers.
//! - [`version`]: GNOME's dotted version comparison.

pub mod autostart;
pub mod components;
pub mod first_login;
pub mod lock;
pub mod login_environment;
pub mod manager;
pub mod restart;
pub mod settings_overlay;
pub mod version;
