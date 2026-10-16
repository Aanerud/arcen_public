use crate::ffi;
use std::ffi::{c_void, CStr};
use std::fmt;

/// Trait for Core Foundation values that can be inserted into CF collections.
pub trait AsCFType {
    /// Borrow the underlying Core Foundation object pointer.
    fn as_ptr(&self) -> *mut c_void;

    /// Clone this value as an erased [`CFType`].
    #[must_use]
    fn to_cf_type(&self) -> CFType {
        let retained = unsafe { ffi::cf_type_retain(self.as_ptr()) };
        CFType::from_raw(retained).expect("retained CFType pointer must be non-null")
    }
}

/// Owned, type-erased `CFTypeRef`.
pub struct CFType(*mut c_void);

impl CFType {
    /// Wraps a +1 retained `CFTypeRef` and returns `None` for null.
    #[must_use]
    pub fn from_raw(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }

    /// Retains a +0 borrowed `CFTypeRef` and wraps the resulting +1 reference.
    ///
    /// # Safety
    ///
    /// `ptr` must be either NULL or a valid `CFTypeRef`.
    #[must_use]
    pub unsafe fn from_raw_retained(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            let retained = unsafe { ffi::cf_type_retain(ptr) };
            Self::from_raw(retained)
        }
    }

    /// Borrow the raw `CFTypeRef` pointer.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Runtime type identifier of the wrapped object.
    #[must_use]
    pub fn type_id(&self) -> usize {
        unsafe { ffi::cf_type_get_type_id(self.0) }
    }

    /// `CFHash` of the wrapped object.
    #[must_use]
    pub fn hash_code(&self) -> usize {
        unsafe { ffi::cf_type_hash(self.0) }
    }

    /// Human-readable Core Foundation description.
    #[must_use]
    pub fn description(&self) -> String {
        let ptr = unsafe { ffi::cf_type_copy_description(self.0) };
        if ptr.is_null() {
            return String::new();
        }
        let string = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::acf_free_string(ptr) };
        string
    }
}

impl Clone for CFType {
    fn clone(&self) -> Self {
        let retained = unsafe { ffi::cf_type_retain(self.0) };
        Self(retained)
    }
}

impl AsCFType for CFType {
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for CFType {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::cf_type_release(self.0) };
        }
    }
}

impl PartialEq for CFType {
    fn eq(&self, other: &Self) -> bool {
        unsafe { ffi::cf_type_equal(self.0, other.0) }
    }
}

impl Eq for CFType {}

impl std::hash::Hash for CFType {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash_code().hash(state);
    }
}

impl fmt::Debug for CFType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CFType")
            .field("ptr", &self.0)
            .field("type_id", &self.type_id())
            .field("description", &self.description())
            .finish()
    }
}

impl fmt::Display for CFType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.description())
    }
}

/// Owned Swift bridge holder object used for callback-heavy wrappers.
pub struct SwiftObject(*mut c_void);

impl SwiftObject {
    /// Wraps a +1 retained bridge object pointer and returns `None` for null.
    #[must_use]
    pub(crate) fn from_raw(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }

    /// Returns the wrapped raw bridge object pointer.
    #[must_use]
    pub(crate) const fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Clone for SwiftObject {
    fn clone(&self) -> Self {
        let retained = unsafe { ffi::acf_object_retain(self.0) };
        Self(retained)
    }
}

impl Drop for SwiftObject {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::acf_object_release(self.0) };
        }
    }
}

impl PartialEq for SwiftObject {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for SwiftObject {}

impl std::hash::Hash for SwiftObject {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        unsafe { ffi::acf_object_hash(self.0) }.hash(state);
    }
}

impl fmt::Debug for SwiftObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SwiftObject").field("ptr", &self.0).finish()
    }
}

macro_rules! impl_cf_type_wrapper {
    ($name:ident, $type_id_fn:ident) => {
        #[derive(Clone, PartialEq, Eq, Hash)]
        #[doc = concat!("Safe wrapper around a retained Core Foundation `", stringify!($name), "` reference.")]
        pub struct $name(pub(crate) crate::cf::base::CFType);

        impl $name {
            #[doc = concat!("Wraps a +1 retained `", stringify!($name), "` pointer and returns `None` for null.")]
            #[must_use]
            pub fn from_raw(ptr: *mut std::ffi::c_void) -> Option<Self> {
                crate::cf::base::CFType::from_raw(ptr).map(Self)
            }

            #[doc = concat!("Retains a +0 borrowed `", stringify!($name), "` pointer and wraps the resulting +1 reference.")]
            ///
            /// # Safety
            ///
            #[doc = concat!("`ptr` must be NULL or a valid `", stringify!($name), "` pointer.")]
            #[must_use]
            pub unsafe fn from_raw_retained(ptr: *mut std::ffi::c_void) -> Option<Self> {
                unsafe { crate::cf::base::CFType::from_raw_retained(ptr) }.map(Self)
            }

            /// Returns the wrapped raw Core Foundation pointer.
            #[must_use]
            pub const fn as_ptr(&self) -> *mut std::ffi::c_void {
                self.0.as_ptr()
            }

            #[doc = concat!("Returns the Core Foundation type ID for `", stringify!($name), "`.")]
            #[must_use]
            pub fn type_id() -> usize {
                unsafe { crate::ffi::$type_id_fn() }
            }

            /// Consumes this wrapper and returns the erased `CFType`.
            #[must_use]
            pub fn into_cf_type(self) -> crate::cf::base::CFType {
                self.0
            }
        }

        impl crate::cf::base::AsCFType for $name {
            fn as_ptr(&self) -> *mut std::ffi::c_void {
                self.as_ptr()
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("ptr", &self.as_ptr())
                    .field("description", &self.0.description())
                    .finish()
            }
        }
    };
}

/// Re-exports the wrapper-generation macro within this crate.
pub(crate) use impl_cf_type_wrapper;
