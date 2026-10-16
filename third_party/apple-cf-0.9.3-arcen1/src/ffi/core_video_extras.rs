#![allow(missing_docs)]

use core::ffi::c_void;

extern "C" {
    /// Swift bridge function `cv_metal_texture_cache_create_system_default` for the corresponding Apple API.
    pub fn cv_metal_texture_cache_create_system_default() -> *mut c_void;
    /// Swift bridge function `cv_metal_texture_cache_flush` for the corresponding Apple API.
    pub fn cv_metal_texture_cache_flush(cache: *mut c_void);
}
