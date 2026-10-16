//! `CGColorSpace` and `CGImage` — the most-used Core Graphics drawing types.
//!
//! These are RAII wrappers around `CFType`-style references with
//! retain/release. Use them with [`crate::cg::CGContext`] for offscreen
//! rasterisation, or for converting between formats via `ImageIO`.

use core::ffi::c_void;
use core::ptr;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::ffi as bridge_ffi;

use super::ffi as cg_ffi;

/// Reference-counted `CGColorSpaceRef`.
pub struct CGColorSpace {
    ptr: *mut c_void,
}

// SAFETY: `CGColorSpaceRef` is an immutable, reference-counted Core Foundation
// object.  All mutations go through Apple's thread-safe retain/release
// primitives; sending or sharing the opaque pointer across threads is safe.
unsafe impl Send for CGColorSpace {}
unsafe impl Sync for CGColorSpace {}

impl Drop for CGColorSpace {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { cg_ffi::CGColorSpaceRelease(self.ptr) };
            self.ptr = ptr::null_mut();
        }
    }
}

impl Clone for CGColorSpace {
    fn clone(&self) -> Self {
        let p = unsafe { cg_ffi::CGColorSpaceRetain(self.ptr) };
        Self { ptr: p }
    }
}

impl std::fmt::Debug for CGColorSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CGColorSpace")
            .field("ptr", &self.ptr)
            .finish()
    }
}

impl CGColorSpace {
    /// Wrap a raw `CGColorSpaceRef` pointer — takes ownership without retaining.
    ///
    /// # Safety
    ///
    /// `ptr` must be a non-null `CGColorSpaceRef` whose ownership the caller is
    /// transferring to the returned [`CGColorSpace`].
    #[must_use]
    pub const unsafe fn from_raw(ptr: *mut c_void) -> Self {
        Self { ptr }
    }

    /// Device RGB.
    #[must_use]
    pub fn device_rgb() -> Self {
        Self {
            ptr: unsafe { cg_ffi::CGColorSpaceCreateDeviceRGB() },
        }
    }

    /// Device gray.
    #[must_use]
    pub fn device_gray() -> Self {
        Self {
            ptr: unsafe { cg_ffi::CGColorSpaceCreateDeviceGray() },
        }
    }

    /// sRGB.
    #[must_use]
    pub fn srgb() -> Self {
        unsafe {
            let n = CFStringCreateWithCStringLite(b"kCGColorSpaceSRGB\0".as_ptr());
            let p = cg_ffi::CGColorSpaceCreateWithName(n);
            CFReleaseLite(n);
            Self { ptr: p }
        }
    }

    /// Display P3.
    #[must_use]
    pub fn display_p3() -> Self {
        unsafe {
            let n = CFStringCreateWithCStringLite(b"kCGColorSpaceDisplayP3\0".as_ptr());
            let p = cg_ffi::CGColorSpaceCreateWithName(n);
            CFReleaseLite(n);
            Self { ptr: p }
        }
    }

    /// Number of color components (`3` for RGB, `1` for gray, …).
    #[must_use]
    pub fn number_of_components(&self) -> usize {
        unsafe { cg_ffi::CGColorSpaceGetNumberOfComponents(self.ptr) }
    }

    /// Raw `CGColorSpaceRef` pointer.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }
}

/// Reference-counted `CGImageRef` — an immutable bitmap.
pub struct CGImage {
    ptr: *mut c_void,
}

// SAFETY: `CGImageRef` is an immutable, reference-counted Core Foundation
// object.  Apple's retain/release primitives are thread-safe, and the image
// data itself is read-only after creation.
unsafe impl Send for CGImage {}
unsafe impl Sync for CGImage {}

impl Drop for CGImage {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { cg_ffi::CGImageRelease(self.ptr) };
            self.ptr = ptr::null_mut();
        }
    }
}

impl Clone for CGImage {
    fn clone(&self) -> Self {
        let p = unsafe { cg_ffi::CGImageRetain(self.ptr) };
        Self { ptr: p }
    }
}

impl std::fmt::Debug for CGImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CGImage").field("ptr", &self.ptr).finish()
    }
}

impl CGImage {
    /// Wrap a raw `CGImageRef` pointer — takes ownership without retaining.
    ///
    /// # Safety
    ///
    /// `ptr` must be a non-null `CGImageRef` whose ownership the caller is
    /// transferring to the returned [`CGImage`].
    #[must_use]
    pub const unsafe fn from_raw(ptr: *mut c_void) -> Self {
        Self { ptr }
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> usize {
        unsafe { cg_ffi::CGImageGetWidth(self.ptr) }
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> usize {
        unsafe { cg_ffi::CGImageGetHeight(self.ptr) }
    }

    /// Bits per component (`8`, `16`, `32`).
    #[must_use]
    pub fn bits_per_component(&self) -> usize {
        unsafe { cg_ffi::CGImageGetBitsPerComponent(self.ptr) }
    }

    /// Bits per pixel.
    #[must_use]
    pub fn bits_per_pixel(&self) -> usize {
        unsafe { cg_ffi::CGImageGetBitsPerPixel(self.ptr) }
    }

    /// Bytes per row.
    #[must_use]
    pub fn bytes_per_row(&self) -> usize {
        unsafe { cg_ffi::CGImageGetBytesPerRow(self.ptr) }
    }

    /// Save the image as a PNG file.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the path contains an interior NUL byte or if
    /// the underlying `ImageIO` export fails.
    pub fn save_png<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let c_path = CString::new(path.as_ref().as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains an interior NUL byte",
            )
        })?;

        if unsafe { bridge_ffi::cgimage_save_png(self.ptr, c_path.as_ptr()) } {
            Ok(())
        } else {
            Err(io::Error::other("cgimage_save_png returned false"))
        }
    }

    /// Raw `CGImageRef` pointer.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }
}

extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        bytes: *const u8,
        encoding: u32,
    ) -> *mut c_void;
    fn CFRelease(cf: *const c_void);
}

#[allow(non_snake_case)]
unsafe fn CFStringCreateWithCStringLite(bytes: *const u8) -> *const c_void {
    CFStringCreateWithCString(ptr::null(), bytes, 0x0800_0100).cast_const()
}

#[allow(non_snake_case)]
unsafe fn CFReleaseLite(p: *const c_void) {
    CFRelease(p);
}
