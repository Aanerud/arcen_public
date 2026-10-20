//! Shared session state and crash-safe restore leases.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod agent_relay;
pub mod auth_throttle;
pub mod compositor_prefix;
pub mod deskside;
pub mod desktop_provider;
pub mod desktop_session;
pub mod direct_reconnect;
pub mod host_lifecycle;
pub mod install_lifecycle;
pub mod install_provider;
pub mod local_playback;
pub mod login_window_handover;
pub mod native_seat;
pub mod network_reachability;
pub mod pier_config;
pub mod polkit_remote;
pub mod restore_lease;
pub mod session_admission;
pub mod session_locale;
pub mod wayland_outputs;
pub mod wayland_qualification;
pub mod wayland_session;
pub mod wayland_x11;
pub mod x11_seat;
pub mod zoneinfo;
