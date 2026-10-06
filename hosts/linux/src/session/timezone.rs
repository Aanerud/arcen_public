//! Linux zoneinfo-backed validation for process-scoped time-zone redirection.

pub use arcen_session::zoneinfo::{
    validate_zoneinfo_timezone, ZoneinfoValidationError as TimezoneValidationError,
};
