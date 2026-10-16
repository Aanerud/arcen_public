#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]
#![allow(improper_ctypes)]
#![allow(clashing_extern_declarations)]
#![allow(clippy::all)]

//! Exhaustive low-level bindings for the Apple Core* / `IOSurface` / Dispatch surface.
//!
//! The higher-level modules in this crate (`cf`, `cm`, `cv`, `dispatch_queue`, `iosurface`)
//! remain the primary ergonomic API. This module exists so downstream crates can reach the
//! full audited system surface directly when they need an exact CoreFoundation/CoreMedia/
//! CoreVideo/IOSurface/Dispatch declaration that does not yet have a bespoke safe wrapper.

#[allow(warnings)]
#[allow(clippy::all)]
mod extras;
#[allow(warnings)]
#[allow(clippy::all)]
mod generated;

pub use extras::*;
pub use generated::*;
