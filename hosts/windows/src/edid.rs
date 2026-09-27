//! Compatibility re-export for the shared EDID/HDR builder.
//!
//! Windows owns the native EDID application path; the byte construction and
//! validation are platform-independent and live in `arcen-outputs`.

pub use arcen_outputs::edid::*;
