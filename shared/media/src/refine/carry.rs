// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Pier's forwarding rule for video-lane refinement records (ADR 0016
//! R2). It lives in [`arcen_protocol::refine::carry`] so the lane relay of a
//! split Pier, which does not depend on this crate, applies the same rule.

pub use arcen_protocol::refine::carry::{MAX_CARRIED_RECORDS, RefineVideoCarry};
