//! Shared session state and crash-safe restore leases.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod deskside;
pub mod direct_reconnect;
pub mod pier_config;
pub mod restore_lease;
