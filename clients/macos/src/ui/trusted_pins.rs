// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Remembered host identities (`trusted_pins.json`): moved to [`arcen_deck_core::trusted_pins`] (ADR 0024 extraction step 1);
//! re-exported so existing paths keep resolving.

pub use arcen_deck_core::trusted_pins::*;
