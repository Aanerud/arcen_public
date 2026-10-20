//! Per-client session state + the WS handshake.

pub mod agent;
pub mod audio;
pub mod auth;
pub mod client;
pub mod handshake;
pub mod identity;
pub mod launcher;
pub mod lifecycle;
pub mod locale;
#[cfg(target_os = "linux")]
pub mod login_env;
pub mod monitor_mux;
pub mod multi_monitor;
pub(crate) mod output_provider;
#[cfg(target_os = "linux")]
pub mod polkit_remote;
pub mod randr_verify;
pub mod resume;
pub mod selinux;
#[cfg(target_os = "linux")]
pub mod sleep_inhibitor;
pub mod timezone;
pub mod wayland;
pub mod xorg_multihead;
