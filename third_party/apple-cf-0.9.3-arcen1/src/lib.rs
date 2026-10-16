//! `apple-cf` — safe, dependency-free Rust bindings for Apple's shared
//! Core* frameworks.
//!
//! This crate is the foundation of the doom-fish macOS Rust suite. It exists
//! so framework-agnostic types like [`cg::CGRect`], [`iosurface::IOSurface`],
//! and [`dispatch_queue::DispatchQueue`] don't have to be re-vendored by every crate that
//! builds on top of `CMSampleBuffer`/`CVPixelBuffer`/etc.
//!
//! # Modules
//!
//! | Module | Framework | Feature flag |
//! |---|---|---|
//! | [`cf`] | Core Foundation value, collection, locale, formatter, and runtime wrappers | — |
//! | [`raw`] | Exhaustive low-level CoreFoundation/CoreMedia/CoreVideo/IOSurface/Dispatch bindings | — |
//! | [`cg`] | CoreGraphics value types + bitmap drawing wrappers | `cg` |
//! | [`iosurface`] | `IOSurface` (zero-copy GPU buffers) | `iosurface` |
//! | [`dispatch_queue`] | Grand Central Dispatch | `dispatch` |
//! | [`cm`] | `CoreMedia` time / sample / buffer wrappers | `cm` |
//! | [`cv`] | `CoreVideo` pixel-buffer wrappers | `cv` |
//! | [`utils`] | shared FFI helpers (always on) | — |
//!
//! # Architecture
//!
//! ```text
//! Safe Rust API (CGRect, CGContext, IOSurface, DispatchQueue, ...)
//!     ├── exhaustive raw bindings (src/raw)
//!     ├── direct Apple framework FFI (src/cg/mod.rs)
//!     └── Swift @_cdecl bridge FFI (src/ffi/mod.rs)
//!             └── swift-bridge/Sources/...
//!                     └── Apple Core* frameworks
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(clashing_extern_declarations)]

mod error;

/// Core Foundation value, collection, and runtime wrappers.
pub mod cf;
/// Low-level FFI declarations used by the safe wrappers.
pub mod ffi;
/// Exhaustive low-level Apple SDK bindings.
pub mod raw;
/// Shared helper utilities and FFI shims.
pub mod utils;

pub use error::CFError;

#[cfg(feature = "cg")]
#[cfg_attr(docsrs, doc(cfg(feature = "cg")))]
/// Core Graphics geometry and bitmap-drawing wrappers.
pub mod cg;

#[cfg(feature = "iosurface")]
#[cfg_attr(docsrs, doc(cfg(feature = "iosurface")))]
/// IOSurface ownership and access wrappers.
pub mod iosurface;

#[cfg(feature = "dispatch")]
#[cfg_attr(docsrs, doc(cfg(feature = "dispatch")))]
/// Grand Central Dispatch queue and synchronization wrappers.
pub mod dispatch_queue;

#[cfg(feature = "cm")]
#[cfg_attr(docsrs, doc(cfg(feature = "cm")))]
/// Core Media time, buffer, and format-description wrappers.
pub mod cm;

#[cfg(feature = "cv")]
#[cfg_attr(docsrs, doc(cfg(feature = "cv")))]
/// Core Video buffer and pixel-buffer wrappers.
pub mod cv;

pub use utils::FourCharCode;

/// Common imports for users of this crate.
pub mod prelude {
    pub use crate::cf::{CFArray, CFString, CFType, CFURL};
    #[cfg(feature = "cg")]
    pub use crate::cg::{CGPoint, CGRect, CGSize};
    #[cfg(feature = "cm")]
    pub use crate::cm::{CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMTimeRange};
    #[cfg(feature = "cv")]
    pub use crate::cv::{
        CVBuffer, CVImageBuffer, CVMetalTextureCache, CVPixelBuffer, CVPixelBufferLockFlags,
    };
    #[cfg(feature = "dispatch")]
    pub use crate::dispatch_queue::{
        DispatchGroup, DispatchQoS, DispatchQueue, DispatchSemaphore, DispatchSource,
    };
    #[cfg(feature = "iosurface")]
    pub use crate::iosurface::{IOSurface, IOSurfaceLockOptions};
    pub use crate::utils::FourCharCode;
}
