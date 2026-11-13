// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod logind_watch;
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
#[cfg(target_os = "linux")]
pub mod socket_dirs;
pub mod timezone;
pub mod wayland;
pub mod xorg_multihead;
