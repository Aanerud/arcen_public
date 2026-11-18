// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Match My Layout's multi-window session, shared by both Decks: one
//! fullscreen window per negotiated monitor (moved from the macOS Deck).
//!
//! - [`session`]: the host's applied topology resolved to local displays,
//!   the pointer mapping and the teardown rule;
//! - [`runtime`]: the plan (which window shows which monitor on which
//!   display) and the transactional enter engine that confirms every
//!   window on its exact display, or aborts the whole layout;
//! - [`activation`]: a `ServerHello`'s applied layout to a plan, and the
//!   session state a Deck holds for it;
//! - [`layout_note`]: the line naming displays a host showed at another
//!   size, or not at all (another product's desktop, kept as it is).
//!
//! A Deck opens the windows (egui viewports on the Mac, Win32 windows on
//! Windows), reports what it observes of each, and acts on the verdicts.

pub mod activation;
pub mod layout_note;
pub mod runtime;
pub mod session;
#[cfg(feature = "dev-tools")]
pub mod simulated_layout;
