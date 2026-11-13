// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Compatibility re-export for the shared EDID/HDR builder.
//!
//! Windows owns the native EDID application path; the byte construction and
//! validation are platform-independent and live in `arcen-outputs`.

pub use arcen_outputs::edid::*;
