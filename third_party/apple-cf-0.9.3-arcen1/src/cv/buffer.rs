//! Core Video buffer/image-buffer wrappers.
//!
#![allow(clippy::missing_errors_doc)]

//! ```rust
//! use apple_cf::cf::{CFString, CFType};
//! use apple_cf::cv::{CVAttachmentMode, CVBuffer, CVImageBuffer, CVPixelBuffer};
//!
//! let pixel_buffer = CVPixelBuffer::create(8, 8, 0x4247_5241).expect("pixel buffer");
//! let buffer = CVBuffer::from_pixel_buffer(&pixel_buffer).expect("buffer");
//! let key = CFString::new("com.doomfish.apple-cf.example");
//! let value = CFString::new("value");
//! buffer.set_attachment(&key, &value, CVAttachmentMode::ShouldPropagate);
//! assert!(buffer.attachment(&key).is_some());
//!
//! let image = CVImageBuffer::from_pixel_buffer(&pixel_buffer).expect("image buffer");
//! assert_eq!(image.encoded_size().width, 8.0);
//! ```

use super::CVPixelBuffer;
use crate::cf::{AsCFType, CFDictionary, CFString, CFType};
use std::ffi::c_void;
use std::fmt;

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVBufferRetain(buffer: *mut c_void) -> *mut c_void;
    fn CVBufferRelease(buffer: *mut c_void);
    fn CVBufferSetAttachment(buffer: *mut c_void, key: *mut c_void, value: *mut c_void, mode: u32);
    fn CVBufferCopyAttachment(
        buffer: *mut c_void,
        key: *mut c_void,
        attachment_mode: *mut u32,
    ) -> *mut c_void;
    fn CVBufferCopyAttachments(buffer: *mut c_void, attachment_mode: u32) -> *mut c_void;
    fn CVBufferRemoveAllAttachments(buffer: *mut c_void);

    fn CVImageBufferGetEncodedSize(image_buffer: *mut c_void) -> CVImageSize;
    fn CVImageBufferGetDisplaySize(image_buffer: *mut c_void) -> CVImageSize;
    fn CVImageBufferGetCleanRect(image_buffer: *mut c_void) -> CVImageRect;
}

/// Attachment propagation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum CVAttachmentMode {
    ShouldNotPropagate = 0,
    ShouldPropagate = 1,
}

/// Size returned by `CVImageBuffer` accessors.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CVImageSize {
    pub width: f64,
    pub height: f64,
}

/// Rectangle returned by `CVImageBufferGetCleanRect`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CVImageRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Generic `CVBufferRef` wrapper.
pub struct CVBuffer(*mut c_void);

impl CVBuffer {
    /// Wraps a +1 retained `CVBufferRef` and returns `None` for null.
    #[must_use]
    pub fn from_raw(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }

    /// Retains a +0 borrowed `CVBufferRef` and wraps the resulting +1 reference.
    ///
    /// # Safety
    ///
    /// `ptr` must be NULL or a valid `CVBufferRef`.
    #[must_use]
    pub unsafe fn from_raw_retained(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            let retained = unsafe { CVBufferRetain(ptr) };
            Self::from_raw(retained)
        }
    }

    /// Wrap a pixel buffer as a generic `CVBuffer`.
    #[must_use]
    pub fn from_pixel_buffer(pixel_buffer: &CVPixelBuffer) -> Option<Self> {
        unsafe { Self::from_raw_retained(pixel_buffer.as_ptr()) }
    }

    /// Borrow the raw `CVBufferRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Attach a Core Foundation value to the buffer.
    pub fn set_attachment(&self, key: &CFString, value: &dyn AsCFType, mode: CVAttachmentMode) {
        unsafe { CVBufferSetAttachment(self.0, key.as_ptr(), value.as_ptr(), mode as u32) };
    }

    /// Copy an attachment value for `key`.
    #[must_use]
    pub fn attachment(&self, key: &CFString) -> Option<CFType> {
        let mut attachment_mode = 0_u32;
        let ptr = unsafe { CVBufferCopyAttachment(self.0, key.as_ptr(), &mut attachment_mode) };
        CFType::from_raw(ptr)
    }

    /// Copy all attachments for the requested propagation mode.
    #[must_use]
    pub fn attachments(&self, mode: CVAttachmentMode) -> Option<CFDictionary> {
        let ptr = unsafe { CVBufferCopyAttachments(self.0, mode as u32) };
        CFDictionary::from_raw(ptr)
    }

    /// Remove all attachments.
    pub fn remove_all_attachments(&self) {
        unsafe { CVBufferRemoveAllAttachments(self.0) };
    }
}

impl Clone for CVBuffer {
    fn clone(&self) -> Self {
        let retained = unsafe { CVBufferRetain(self.0) };
        Self(retained)
    }
}

impl Drop for CVBuffer {
    fn drop(&mut self) {
        unsafe { CVBufferRelease(self.0) };
    }
}

impl PartialEq for CVBuffer {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for CVBuffer {}

impl std::hash::Hash for CVBuffer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for CVBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CVBuffer").field("ptr", &self.0).finish()
    }
}

/// Generic `CVImageBufferRef` wrapper.
pub struct CVImageBuffer(*mut c_void);

impl CVImageBuffer {
    /// Wraps a +1 retained `CVImageBufferRef` and returns `None` for null.
    #[must_use]
    pub fn from_raw(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }

    /// Wrap a pixel buffer as a generic image buffer.
    #[must_use]
    pub fn from_pixel_buffer(pixel_buffer: &CVPixelBuffer) -> Option<Self> {
        let retained = unsafe { CVBufferRetain(pixel_buffer.as_ptr()) };
        Self::from_raw(retained)
    }

    /// Borrow the raw `CVImageBufferRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Encoded image size.
    #[must_use]
    pub fn encoded_size(&self) -> CVImageSize {
        unsafe { CVImageBufferGetEncodedSize(self.0) }
    }

    /// Display size.
    #[must_use]
    pub fn display_size(&self) -> CVImageSize {
        unsafe { CVImageBufferGetDisplaySize(self.0) }
    }

    /// Clean aperture rectangle.
    #[must_use]
    pub fn clean_rect(&self) -> CVImageRect {
        unsafe { CVImageBufferGetCleanRect(self.0) }
    }
}

impl Clone for CVImageBuffer {
    fn clone(&self) -> Self {
        let retained = unsafe { CVBufferRetain(self.0) };
        Self(retained)
    }
}

impl Drop for CVImageBuffer {
    fn drop(&mut self) {
        unsafe { CVBufferRelease(self.0) };
    }
}

impl PartialEq for CVImageBuffer {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for CVImageBuffer {}

impl std::hash::Hash for CVImageBuffer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for CVImageBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CVImageBuffer")
            .field("ptr", &self.0)
            .field("encoded_size", &self.encoded_size())
            .finish()
    }
}
