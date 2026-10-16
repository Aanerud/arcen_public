//! Shared utilities used by other modules in this crate and by downstream
//! crates that build on top of the `apple-cf` bridge.
//!
//! Since v0.7.0 these helpers live in the framework-agnostic
//! `doom-fish-utils` crate. This module preserves the historic
//! `apple_cf::utils::*` paths as re-exports plus a thin
//! [`ffi_string`] convenience layer that bakes in `acf_free_string`
//! for the string-owning helpers.

pub use doom_fish_utils::completion;
pub use doom_fish_utils::four_char_code;
pub use doom_fish_utils::four_char_code::FourCharCode;
pub use doom_fish_utils::panic_safe;

/// FFI string helpers that free bridge-owned strings.
pub mod ffi_string;
