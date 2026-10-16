//! `CGContext` — offscreen bitmap drawing with Core Graphics.

use core::ffi::c_void;
use core::ptr;

use crate::CFError;

use super::drawing::{CGColorSpace, CGImage};
use super::ffi;
use super::CGRect;

const BITS_PER_COMPONENT_8: usize = 8;
const CG_IMAGE_ALPHA_NONE: u32 = 0;
const CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;
const CG_BITMAP_BYTE_ORDER_32_BIG: u32 = 16_384;
const RGBA8_BITMAP_INFO: u32 = CG_IMAGE_ALPHA_PREMULTIPLIED_LAST | CG_BITMAP_BYTE_ORDER_32_BIG;
const GRAYSCALE8_BITMAP_INFO: u32 = CG_IMAGE_ALPHA_NONE;

/// Reference-counted `CGContextRef` backed by a bitmap.
#[derive(Debug)]
pub struct CGContext {
    ptr: *mut c_void,
}

impl Drop for CGContext {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::CGContextRelease(self.ptr) };
            self.ptr = ptr::null_mut();
        }
    }
}

impl Clone for CGContext {
    fn clone(&self) -> Self {
        Self {
            ptr: unsafe { ffi::CGContextRetain(self.ptr) },
        }
    }
}

impl CGContext {
    /// Wrap a raw `CGContextRef` pointer — takes ownership without retaining.
    ///
    /// # Safety
    ///
    /// `ptr` must be a non-null `CGContextRef` whose ownership the caller is
    /// transferring to the returned [`CGContext`].
    #[must_use]
    pub const unsafe fn from_raw(ptr: *mut c_void) -> Self {
        Self { ptr }
    }

    fn new_bitmap(
        width: usize,
        height: usize,
        color_space: &CGColorSpace,
        bitmap_info: u32,
    ) -> Result<Self, CFError> {
        let context = unsafe {
            ffi::CGBitmapContextCreate(
                ptr::null_mut(),
                width,
                height,
                BITS_PER_COMPONENT_8,
                0,
                color_space.as_ptr(),
                bitmap_info,
            )
        };

        if context.is_null() {
            Err(CFError::new("CGBitmapContextCreate"))
        } else {
            Ok(Self { ptr: context })
        }
    }

    /// Create a premultiplied-last RGBA8 bitmap context.
    ///
    /// # Errors
    ///
    /// Returns [`CFError`] if Core Graphics fails to create the bitmap context.
    pub fn new_rgba8(width: usize, height: usize) -> Result<Self, CFError> {
        let color_space = CGColorSpace::device_rgb();
        Self::new_bitmap(width, height, &color_space, RGBA8_BITMAP_INFO)
    }

    /// Create an 8-bit grayscale bitmap context.
    ///
    /// # Errors
    ///
    /// Returns [`CFError`] if Core Graphics fails to create the bitmap context.
    pub fn new_grayscale(width: usize, height: usize) -> Result<Self, CFError> {
        let color_space = CGColorSpace::device_gray();
        Self::new_bitmap(width, height, &color_space, GRAYSCALE8_BITMAP_INFO)
    }

    fn buffer_len(&self) -> usize {
        self.height().checked_mul(self.bytes_per_row()).unwrap_or(0)
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> usize {
        unsafe { ffi::CGBitmapContextGetWidth(self.ptr) }
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> usize {
        unsafe { ffi::CGBitmapContextGetHeight(self.ptr) }
    }

    /// Bytes per row.
    #[must_use]
    pub fn bytes_per_row(&self) -> usize {
        unsafe { ffi::CGBitmapContextGetBytesPerRow(self.ptr) }
    }

    /// Bits per component.
    #[must_use]
    pub fn bits_per_component(&self) -> usize {
        unsafe { ffi::CGBitmapContextGetBitsPerComponent(self.ptr) }
    }

    /// Bits per pixel.
    #[must_use]
    pub fn bits_per_pixel(&self) -> usize {
        unsafe { ffi::CGBitmapContextGetBitsPerPixel(self.ptr) }
    }

    /// Raw bitmap data pointer.
    #[must_use]
    pub fn data(&self) -> *mut u8 {
        unsafe { ffi::CGBitmapContextGetData(self.ptr).cast::<u8>() }
    }

    /// The context color space, if one is set.
    #[must_use]
    pub fn color_space(&self) -> Option<CGColorSpace> {
        unsafe {
            let color_space = ffi::CGBitmapContextGetColorSpace(self.ptr);
            if color_space.is_null() {
                None
            } else {
                Some(CGColorSpace::from_raw(ffi::CGColorSpaceRetain(color_space)))
            }
        }
    }

    /// The raw `CGImageAlphaInfo` value for the bitmap context.
    #[must_use]
    pub fn alpha_info(&self) -> u32 {
        unsafe { ffi::CGBitmapContextGetAlphaInfo(self.ptr) }
    }

    /// The bitmap storage as immutable bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        let data = self.data();
        let len = self.buffer_len();
        if data.is_null() || len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(data.cast_const(), len) }
        }
    }

    /// The bitmap storage as mutable bytes.
    #[must_use]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let data = self.data();
        let len = self.buffer_len();
        if data.is_null() || len == 0 {
            &mut []
        } else {
            unsafe { std::slice::from_raw_parts_mut(data, len) }
        }
    }

    /// Set the current fill color in `DeviceRGB`.
    pub fn set_rgb_fill_color(&self, r: f64, g: f64, b: f64, a: f64) {
        unsafe { ffi::CGContextSetRGBFillColor(self.ptr, r, g, b, a) };
    }

    /// Set the current stroke color in `DeviceRGB`.
    pub fn set_rgb_stroke_color(&self, r: f64, g: f64, b: f64, a: f64) {
        unsafe { ffi::CGContextSetRGBStrokeColor(self.ptr, r, g, b, a) };
    }

    /// Set the stroke line width.
    pub fn set_line_width(&self, w: f64) {
        unsafe { ffi::CGContextSetLineWidth(self.ptr, w) };
    }

    /// Clear a rectangle to transparent black.
    pub fn clear_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        unsafe { ffi::CGContextClearRect(self.ptr, CGRect::new(x, y, w, h)) };
    }

    /// Fill a rectangle.
    pub fn fill_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        unsafe { ffi::CGContextFillRect(self.ptr, CGRect::new(x, y, w, h)) };
    }

    /// Stroke a rectangle.
    pub fn stroke_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        unsafe { ffi::CGContextStrokeRect(self.ptr, CGRect::new(x, y, w, h)) };
    }

    /// Begin a new path.
    pub fn begin_path(&self) {
        unsafe { ffi::CGContextBeginPath(self.ptr) };
    }

    /// Close the current path.
    pub fn close_path(&self) {
        unsafe { ffi::CGContextClosePath(self.ptr) };
    }

    /// Move the current point.
    pub fn move_to(&self, x: f64, y: f64) {
        unsafe { ffi::CGContextMoveToPoint(self.ptr, x, y) };
    }

    /// Add a line segment to the current path.
    pub fn add_line_to(&self, x: f64, y: f64) {
        unsafe { ffi::CGContextAddLineToPoint(self.ptr, x, y) };
    }

    /// Add a rectangle to the current path.
    pub fn add_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        unsafe { ffi::CGContextAddRect(self.ptr, CGRect::new(x, y, w, h)) };
    }

    /// Add an ellipse inscribed in the rectangle.
    pub fn add_ellipse_in_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        unsafe { ffi::CGContextAddEllipseInRect(self.ptr, CGRect::new(x, y, w, h)) };
    }

    /// Fill the current path.
    pub fn fill_path(&self) {
        unsafe { ffi::CGContextFillPath(self.ptr) };
    }

    /// Stroke the current path.
    pub fn stroke_path(&self) {
        unsafe { ffi::CGContextStrokePath(self.ptr) };
    }

    /// Draw an image into the target rectangle.
    pub fn draw_image(&self, x: f64, y: f64, w: f64, h: f64, image: &CGImage) {
        unsafe { ffi::CGContextDrawImage(self.ptr, CGRect::new(x, y, w, h), image.as_ptr()) };
    }

    /// Translate the current transformation matrix.
    pub fn translate(&self, tx: f64, ty: f64) {
        unsafe { ffi::CGContextTranslateCTM(self.ptr, tx, ty) };
    }

    /// Scale the current transformation matrix.
    pub fn scale(&self, sx: f64, sy: f64) {
        unsafe { ffi::CGContextScaleCTM(self.ptr, sx, sy) };
    }

    /// Rotate the current transformation matrix.
    pub fn rotate(&self, radians: f64) {
        unsafe { ffi::CGContextRotateCTM(self.ptr, radians) };
    }

    /// Save the current graphics state.
    pub fn save_g_state(&self) {
        unsafe { ffi::CGContextSaveGState(self.ptr) };
    }

    /// Restore the most recently saved graphics state.
    pub fn restore_g_state(&self) {
        unsafe { ffi::CGContextRestoreGState(self.ptr) };
    }

    /// Snapshot the current bitmap contents to a `CGImage`.
    #[must_use]
    pub fn snapshot_to_image(&self) -> Option<CGImage> {
        let image = unsafe { ffi::CGBitmapContextCreateImage(self.ptr) };
        if image.is_null() {
            None
        } else {
            Some(unsafe { CGImage::from_raw(image) })
        }
    }

    /// Raw `CGContextRef` pointer.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }
}
