// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Linux zoneinfo-backed validation for process-scoped time-zone redirection.

pub use arcen_session::zoneinfo::{
    validate_zoneinfo_timezone, ZoneinfoValidationError as TimezoneValidationError,
};
