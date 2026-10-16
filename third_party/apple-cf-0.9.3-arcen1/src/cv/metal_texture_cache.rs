//! `CVMetalTextureCache` wrapper.
//!
#![allow(clippy::missing_panics_doc)]

//! ```rust,no_run
//! use apple_cf::cv::CVMetalTextureCache;
//!
//! if let Some(cache) = CVMetalTextureCache::system_default() {
//!     cache.flush();
//! }
//! ```

use crate::ffi;
use std::ffi::c_void;
use std::fmt;

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVMetalTextureCacheGetTypeID() -> usize;
}

/// Owned wrapper around `CVMetalTextureCacheRef`.
pub struct CVMetalTextureCache(*mut c_void);

impl CVMetalTextureCache {
    /// Create a texture cache using the system-default Metal device.
    #[must_use]
    pub fn system_default() -> Option<Self> {
        let ptr = unsafe { ffi::cv_metal_texture_cache_create_system_default() };
        Self::from_raw(ptr)
    }

    /// Wraps a +1 retained `CVMetalTextureCacheRef` and returns `None` for null.
    #[must_use]
    pub fn from_raw(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }

    /// Borrow the raw cache pointer.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Core Video type identifier.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { CVMetalTextureCacheGetTypeID() }
    }

    /// Flush pending cached textures.
    pub fn flush(&self) {
        unsafe { ffi::cv_metal_texture_cache_flush(self.0) };
    }
}

impl Clone for CVMetalTextureCache {
    fn clone(&self) -> Self {
        let retained = unsafe { ffi::cf_type_retain(self.0) };
        Self(retained)
    }
}

impl Drop for CVMetalTextureCache {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::cf_type_release(self.0) };
        }
    }
}

impl PartialEq for CVMetalTextureCache {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for CVMetalTextureCache {}

impl std::hash::Hash for CVMetalTextureCache {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        unsafe { ffi::cf_type_hash(self.0) }.hash(state);
    }
}

impl fmt::Debug for CVMetalTextureCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CVMetalTextureCache")
            .field("ptr", &self.0)
            .field("type_id", &Self::type_id())
            .finish()
    }
}
