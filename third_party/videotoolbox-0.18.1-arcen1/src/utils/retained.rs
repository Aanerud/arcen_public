//! Declarative macro for retain/release `Drop` boilerplate.
//!
//! Many wrapper types hold a single pointer to a retained CoreFoundation /
//! `VideoToolbox` object and hand-roll identical `Drop` implementations that
//! null-check the pointer and call `CFRelease` (some after an `Invalidate` /
//! `Close` step first). `vt_retained!` consolidates that boilerplate into a
//! single audited place.
//!
//! The generated impls preserve the behavior of the previous hand-written
//! versions:
//! - `Drop` null-checks the pointer before releasing (matching the original
//!   `if !ptr.is_null()` guards).
//! - The optional `invalidate` step runs before `release`, matching the
//!   `VT*SessionInvalidate` / `VTMultiPassStorageClose` + `CFRelease`
//!   ordering of the session types.
//!
//! The previous hand-written `Drop`s also reset the field to null after
//! releasing; that assignment is unobservable (the value is being dropped and
//! the raw-pointer field has no drop glue), so — like `screencapturekit`'s
//! `sc_retained!` — it is intentionally omitted.
//!
//! Types whose `Drop` carries extra logic beyond release + null-check (e.g.
//! `CompressionSession`'s intentional ref-con leak, `RawProcessingSession`'s
//! parameter-handler teardown, `TaggedBufferGroup`'s retaining `Clone`, or
//! `FrameProcessor`'s custom release functions) are intentionally left
//! hand-written.

/// Generate a `Drop` impl for a retain/release pointer wrapper.
///
/// The `release` path is invoked as `release(self.<field>.cast())`, matching
/// the `CFRelease(ptr.cast())` convention used throughout the crate.
///
/// Variants:
/// - `Drop` only:
///   `vt_retained!(Ty, field = inner, release = crate::ffi::CFRelease);`
/// - `Drop` with an invalidate/close step first:
///   `vt_retained!(Ty, field = session, invalidate = crate::ffi::VTFooSessionInvalidate, release = crate::ffi::CFRelease);`
macro_rules! vt_retained {
    // Drop only.
    ($ty:ty, field = $field:ident, release = $release:path $(,)?) => {
        impl Drop for $ty {
            fn drop(&mut self) {
                if !self.$field.is_null() {
                    unsafe {
                        $release(self.$field.cast());
                    }
                }
            }
        }
    };

    // Drop with an invalidate step before release.
    ($ty:ty, field = $field:ident, invalidate = $invalidate:path, release = $release:path $(,)?) => {
        impl Drop for $ty {
            fn drop(&mut self) {
                if !self.$field.is_null() {
                    unsafe {
                        $invalidate(self.$field);
                        $release(self.$field.cast());
                    }
                }
            }
        }
    };
}

pub(crate) use vt_retained;
