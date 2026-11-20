// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Network layer: the TLS/WebSocket listener + per-connection relay.

pub(crate) mod joined_input;
pub(crate) mod joined_pen;
pub mod server;
pub(crate) mod setup_watch;
pub mod tls;

pub use server::serve;
