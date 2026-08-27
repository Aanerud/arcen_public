#![allow(warnings)]
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
mod guid;
mod version;
#[rustfmt::skip]
mod windows_sys;
pub use guid::*;
pub use version::*;
pub use windows_sys::*;
