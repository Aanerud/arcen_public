//! Core Graphics types for screen coordinates, dimensions, and bitmap drawing.
//!
//! This module provides Rust equivalents of Core Graphics types used in
//! `ScreenCaptureKit` plus safe wrappers for the most common offscreen drawing
//! APIs (`CGColorSpace`, `CGImage`, and `CGContext`).

mod affine;
mod context;
mod drawing;
mod point;
mod rect;
mod size;

/// Low-level FFI declarations used by the safe wrappers.
pub(crate) mod ffi {
    use core::ffi::c_void;

    use super::rect::CGRect;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        /// Apple SDK function `CGColorSpaceCreateDeviceRGB`.
        pub(crate) fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        /// Apple SDK function `CGColorSpaceCreateDeviceGray`.
        pub(crate) fn CGColorSpaceCreateDeviceGray() -> *mut c_void;
        /// Apple SDK function `CGColorSpaceCreateWithName`.
        pub(crate) fn CGColorSpaceCreateWithName(name: *const c_void) -> *mut c_void;
        /// Apple SDK function `CGColorSpaceRelease`.
        pub(crate) fn CGColorSpaceRelease(cs: *mut c_void);
        /// Apple SDK function `CGColorSpaceRetain`.
        pub(crate) fn CGColorSpaceRetain(cs: *mut c_void) -> *mut c_void;
        /// Apple SDK function `CGColorSpaceGetNumberOfComponents`.
        pub(crate) fn CGColorSpaceGetNumberOfComponents(cs: *mut c_void) -> usize;

        /// Apple SDK function `CGImageGetWidth`.
        pub(crate) fn CGImageGetWidth(image: *mut c_void) -> usize;
        /// Apple SDK function `CGImageGetHeight`.
        pub(crate) fn CGImageGetHeight(image: *mut c_void) -> usize;
        /// Apple SDK function `CGImageGetBitsPerComponent`.
        pub(crate) fn CGImageGetBitsPerComponent(image: *mut c_void) -> usize;
        /// Apple SDK function `CGImageGetBitsPerPixel`.
        pub(crate) fn CGImageGetBitsPerPixel(image: *mut c_void) -> usize;
        /// Apple SDK function `CGImageGetBytesPerRow`.
        pub(crate) fn CGImageGetBytesPerRow(image: *mut c_void) -> usize;
        /// Apple SDK function `CGImageRelease`.
        pub(crate) fn CGImageRelease(image: *mut c_void);
        /// Apple SDK function `CGImageRetain`.
        pub(crate) fn CGImageRetain(image: *mut c_void) -> *mut c_void;

        /// Apple SDK function `CGBitmapContextCreate`.
        pub(crate) fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            bitmap_info: u32,
        ) -> *mut c_void;
        /// Apple SDK function `CGBitmapContextGetData`.
        pub(crate) fn CGBitmapContextGetData(context: *mut c_void) -> *mut c_void;
        /// Apple SDK function `CGBitmapContextGetWidth`.
        pub(crate) fn CGBitmapContextGetWidth(context: *mut c_void) -> usize;
        /// Apple SDK function `CGBitmapContextGetHeight`.
        pub(crate) fn CGBitmapContextGetHeight(context: *mut c_void) -> usize;
        /// Apple SDK function `CGBitmapContextGetBitsPerComponent`.
        pub(crate) fn CGBitmapContextGetBitsPerComponent(context: *mut c_void) -> usize;
        /// Apple SDK function `CGBitmapContextGetBitsPerPixel`.
        pub(crate) fn CGBitmapContextGetBitsPerPixel(context: *mut c_void) -> usize;
        /// Apple SDK function `CGBitmapContextGetBytesPerRow`.
        pub(crate) fn CGBitmapContextGetBytesPerRow(context: *mut c_void) -> usize;
        /// Apple SDK function `CGBitmapContextGetColorSpace`.
        pub(crate) fn CGBitmapContextGetColorSpace(context: *mut c_void) -> *mut c_void;
        /// Apple SDK function `CGBitmapContextGetAlphaInfo`.
        pub(crate) fn CGBitmapContextGetAlphaInfo(context: *mut c_void) -> u32;
        /// Apple SDK function `CGBitmapContextCreateImage`.
        pub(crate) fn CGBitmapContextCreateImage(context: *mut c_void) -> *mut c_void;

        /// Apple SDK function `CGContextRetain`.
        pub(crate) fn CGContextRetain(context: *mut c_void) -> *mut c_void;
        /// Apple SDK function `CGContextRelease`.
        pub(crate) fn CGContextRelease(context: *mut c_void);
        /// Apple SDK function `CGContextSetRGBFillColor`.
        pub(crate) fn CGContextSetRGBFillColor(
            context: *mut c_void,
            red: f64,
            green: f64,
            blue: f64,
            alpha: f64,
        );
        /// Apple SDK function `CGContextSetRGBStrokeColor`.
        pub(crate) fn CGContextSetRGBStrokeColor(
            context: *mut c_void,
            red: f64,
            green: f64,
            blue: f64,
            alpha: f64,
        );
        /// Apple SDK function `CGContextSetLineWidth`.
        pub(crate) fn CGContextSetLineWidth(context: *mut c_void, width: f64);
        /// Apple SDK function `CGContextFillRect`.
        pub(crate) fn CGContextFillRect(context: *mut c_void, rect: CGRect);
        /// Apple SDK function `CGContextStrokeRect`.
        pub(crate) fn CGContextStrokeRect(context: *mut c_void, rect: CGRect);
        /// Apple SDK function `CGContextFillPath`.
        pub(crate) fn CGContextFillPath(context: *mut c_void);
        /// Apple SDK function `CGContextStrokePath`.
        pub(crate) fn CGContextStrokePath(context: *mut c_void);
        /// Apple SDK function `CGContextClearRect`.
        pub(crate) fn CGContextClearRect(context: *mut c_void, rect: CGRect);
        /// Apple SDK function `CGContextMoveToPoint`.
        pub(crate) fn CGContextMoveToPoint(context: *mut c_void, x: f64, y: f64);
        /// Apple SDK function `CGContextAddLineToPoint`.
        pub(crate) fn CGContextAddLineToPoint(context: *mut c_void, x: f64, y: f64);
        /// Apple SDK function `CGContextAddRect`.
        pub(crate) fn CGContextAddRect(context: *mut c_void, rect: CGRect);
        /// Apple SDK function `CGContextAddEllipseInRect`.
        pub(crate) fn CGContextAddEllipseInRect(context: *mut c_void, rect: CGRect);
        /// Apple SDK function `CGContextBeginPath`.
        pub(crate) fn CGContextBeginPath(context: *mut c_void);
        /// Apple SDK function `CGContextClosePath`.
        pub(crate) fn CGContextClosePath(context: *mut c_void);
        /// Apple SDK function `CGContextDrawImage`.
        pub(crate) fn CGContextDrawImage(context: *mut c_void, rect: CGRect, image: *mut c_void);
        /// Apple SDK function `CGContextTranslateCTM`.
        pub(crate) fn CGContextTranslateCTM(context: *mut c_void, tx: f64, ty: f64);
        /// Apple SDK function `CGContextScaleCTM`.
        pub(crate) fn CGContextScaleCTM(context: *mut c_void, sx: f64, sy: f64);
        /// Apple SDK function `CGContextRotateCTM`.
        pub(crate) fn CGContextRotateCTM(context: *mut c_void, radians: f64);
        /// Apple SDK function `CGContextSaveGState`.
        pub(crate) fn CGContextSaveGState(context: *mut c_void);
        /// Apple SDK function `CGContextRestoreGState`.
        pub(crate) fn CGContextRestoreGState(context: *mut c_void);
    }
}

#[doc(inline)]
pub use crate::raw::CGCharCode;
#[doc(inline)]
pub use crate::raw::CGContextRef;
#[doc(inline)]
pub use crate::raw::CGKeyCode;
pub use affine::{CGAffineTransform, CGVector};
pub use context::CGContext;
pub use drawing::{CGColorSpace, CGImage};
pub use point::CGPoint;
pub use rect::CGRect;
pub use size::CGSize;

/// `CGDisplayID` type alias
pub type CGDisplayID = u32;
