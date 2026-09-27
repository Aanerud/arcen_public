//! Shared session state and crash-safe restore leases.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod agent_relay;
pub mod auth_throttle;
pub mod deskside;
pub mod direct_reconnect;
pub mod host_lifecycle;
pub mod install_lifecycle;
pub mod login_window_handover;
pub mod pier_config;
pub mod restore_lease;
pub mod session_admission;
