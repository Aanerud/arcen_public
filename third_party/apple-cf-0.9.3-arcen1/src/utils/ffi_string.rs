//! FFI string utilities — back-compat shim over `doom_fish_utils::ffi_string`.
//!
//! Re-exports the generic, framework-agnostic helpers and adds two thin
//! wrappers ([`ffi_string_owned`], [`ffi_string_owned_or_empty`]) that
//! bake in `crate::ffi::acf_free_string` as the deallocator so existing
//! call sites that used the apple-cf-specific helpers compile unchanged.

pub use doom_fish_utils::ffi_string::{
    ffi_string_from_buffer, ffi_string_from_buffer_or_empty, DEFAULT_BUFFER_SIZE, SMALL_BUFFER_SIZE,
};

/// Retrieves a string from an FFI function that returns an owned C string
/// pointer allocated by Swift (typically via `strdup`), freeing it with
/// `acf_free_string`.
///
/// This is a thin convenience over
/// [`doom_fish_utils::ffi_string::ffi_string_owned`].
///
/// # Safety
/// The caller must ensure the returned pointer was allocated by the
/// `apple-cf` Swift bridge (so that `acf_free_string` correctly releases
/// it). See the underlying helper for full safety contract.
pub unsafe fn ffi_string_owned<F>(ffi_call: F) -> Option<String>
where
    F: FnOnce() -> *mut i8,
{
    unsafe {
        doom_fish_utils::ffi_string::ffi_string_owned(ffi_call, |ptr| {
            crate::ffi::acf_free_string(ptr);
        })
    }
}

/// Same as [`ffi_string_owned`] but returns an empty string on failure.
///
/// # Safety
/// Same requirements as [`ffi_string_owned`].
pub unsafe fn ffi_string_owned_or_empty<F>(ffi_call: F) -> String
where
    F: FnOnce() -> *mut i8,
{
    unsafe { ffi_string_owned(ffi_call) }.unwrap_or_default()
}
