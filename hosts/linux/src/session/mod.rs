//! Per-client session state + the WS handshake.

pub mod agent;
pub mod audio;
pub mod auth;
pub mod client;
pub mod handshake;
pub mod identity;
pub mod launcher;
pub mod lifecycle;
pub mod monitor_mux;
pub mod multi_monitor;
pub(crate) mod output_provider;
pub mod randr_verify;
pub mod resume;
pub mod timezone;
pub mod xorg_multihead;
