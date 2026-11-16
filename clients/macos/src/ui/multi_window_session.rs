// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Production applied-topology adoption, absolute-pointer coordinate
//! mapping, and teardown detection for a real multi-monitor-v1 session:
//! moved to [`arcen_deck_core::multi_window::session`] so both Decks share
//! it (the local display id is the `CGDirectDisplayID` here); re-exported so
//! existing paths keep resolving.

pub use arcen_deck_core::multi_window::session::*;
