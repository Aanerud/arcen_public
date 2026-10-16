//! Core Video types and wrappers.
//!
//! Provides safe Rust wrappers for `CVPixelBuffer` and `CVPixelBufferPool`,
//! the `CoreVideo` primitives that pair an `IOSurface` with format metadata
//! (pixel format, dimensions, planes). Required for any pipeline that
//! talks to `VideoToolbox` decoders, Vision, or `AVFoundation` capture.

mod buffer;
mod metal_texture_cache;
mod pixel_buffer;

pub use buffer::{CVAttachmentMode, CVBuffer, CVImageBuffer, CVImageRect, CVImageSize};
pub use metal_texture_cache::CVMetalTextureCache;
pub use pixel_buffer::{
    CVPixelBuffer, CVPixelBufferLockFlags, CVPixelBufferLockGuard, CVPixelBufferPool,
    PixelBufferCursorExt,
};
