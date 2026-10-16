//! Core Foundation collection wrappers.
//!
#![allow(clippy::missing_panics_doc)]

//! ```rust
//! use apple_cf::cf::{
//!     CFArray, CFAttributedString, CFBag, CFDictionary, CFMutableSet, CFSet,
//!     CFSetCallbacks, CFString, CFTree,
//! };
//!
//! let first = CFString::new("first");
//! let second = CFString::new("second");
//! let array = CFArray::from_values(&[&first, &second]);
//! assert_eq!(array.len(), 2);
//!
//! let dict = CFDictionary::from_pairs(&[(&first, &second)]);
//! assert!(dict.contains_key(&first));
//!
//! let bag = CFBag::from_values(&[&first, &first, &second]);
//! assert_eq!(bag.count_of_value(&first), 2);
//!
//! let set = CFSet::from_values(&[&first, &second]);
//! assert!(set.contains(&first));
//!
//! let mutable_set = CFMutableSet::with_callbacks(0, CFSetCallbacks::Type);
//! mutable_set.add(&first);
//! mutable_set.set(&second);
//! assert_eq!(mutable_set.len(), 2);
//!
//! let attributed = CFAttributedString::new(&first);
//! assert_eq!(attributed.string().to_string(), "first");
//!
//! let root = CFTree::new(Some(&first));
//! let child = CFTree::new(Some(&second));
//! root.append_child(&child);
//! assert_eq!(root.child_count(), 1);
//! ```

use super::base::{impl_cf_type_wrapper, AsCFType, CFType, SwiftObject};
use super::CFString;
use crate::{ffi, utils::panic_safe};
use std::ffi::c_void;
use std::fmt;

impl_cf_type_wrapper!(CFArray, cf_array_get_type_id);
impl_cf_type_wrapper!(CFDictionary, cf_dictionary_get_type_id);
/// Alias for `CFDictionary`.
pub type CFDict = CFDictionary;
impl_cf_type_wrapper!(CFBag, cf_bag_get_type_id);
impl_cf_type_wrapper!(CFSet, cf_set_get_type_id);
impl_cf_type_wrapper!(CFMutableSet, cf_set_get_type_id);
impl_cf_type_wrapper!(CFAttributedString, cf_attributed_string_get_type_id);

/// Safe representation of the `CFSetCallBacks` / `kCFTypeSetCallBacks` /
/// `kCFCopyStringSetCallBacks` configuration used when creating sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum CFSetCallbacks {
    /// Use Core Foundation retain/release/hash/equality callbacks for arbitrary `CFType`s.
    #[default]
    Type = 0,
    /// Copy inserted `CFString` values using `kCFCopyStringSetCallBacks`.
    CopyString = 1,
}

type CFSetApplyTask<'a> = Box<dyn FnMut(CFType) + 'a>;

extern "C" fn cf_set_apply_trampoline(value: *mut c_void, context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let callback = unsafe { &mut *context.cast::<CFSetApplyTask<'_>>() };
    if let Some(value) = CFType::from_raw(value) {
        panic_safe::catch_user_panic("CFSet::for_each", || callback(value));
    }
}

impl CFArray {
    /// Create an array from borrowed Core Foundation values.
    #[must_use]
    pub fn from_values(values: &[&dyn AsCFType]) -> Self {
        let raw_values: Vec<*mut std::ffi::c_void> =
            values.iter().map(|value| value.as_ptr()).collect();
        let ptr = unsafe { ffi::cf_array_create(raw_values.as_ptr(), raw_values.len()) };
        Self::from_raw(ptr).expect("CFArrayCreate returned NULL")
    }

    /// Number of elements in the array.
    #[must_use]
    pub fn len(&self) -> usize {
        unsafe { ffi::cf_array_get_count(self.as_ptr()) }
    }

    /// Whether the array is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy the value at `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<CFType> {
        let ptr = unsafe { ffi::cf_array_get_value_at_index(self.as_ptr(), index) };
        CFType::from_raw(ptr)
    }

    /// Copy all elements into a Rust vector.
    #[must_use]
    pub fn values(&self) -> Vec<CFType> {
        (0..self.len())
            .filter_map(|index| self.get(index))
            .collect()
    }
}

impl CFDictionary {
    /// Create a dictionary from borrowed key/value pairs.
    #[must_use]
    pub fn from_pairs(pairs: &[(&dyn AsCFType, &dyn AsCFType)]) -> Self {
        let keys: Vec<*mut std::ffi::c_void> = pairs.iter().map(|(key, _)| key.as_ptr()).collect();
        let values: Vec<*mut std::ffi::c_void> =
            pairs.iter().map(|(_, value)| value.as_ptr()).collect();
        let ptr = unsafe { ffi::cf_dictionary_create(keys.as_ptr(), values.as_ptr(), pairs.len()) };
        Self::from_raw(ptr).expect("CFDictionaryCreate returned NULL")
    }

    /// Number of key/value pairs.
    #[must_use]
    pub fn len(&self) -> usize {
        unsafe { ffi::cf_dictionary_get_count(self.as_ptr()) }
    }

    /// Whether the dictionary is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `key` exists in the dictionary.
    #[must_use]
    pub fn contains_key(&self, key: &dyn AsCFType) -> bool {
        unsafe { ffi::cf_dictionary_contains_key(self.as_ptr(), key.as_ptr()) }
    }

    /// Copy the value associated with `key`.
    #[must_use]
    pub fn get(&self, key: &dyn AsCFType) -> Option<CFType> {
        let ptr = unsafe { ffi::cf_dictionary_get_value(self.as_ptr(), key.as_ptr()) };
        CFType::from_raw(ptr)
    }

    /// Copy all keys into a `CFArray`.
    #[must_use]
    pub fn keys(&self) -> CFArray {
        let ptr = unsafe { ffi::cf_dictionary_copy_keys(self.as_ptr()) };
        CFArray::from_raw(ptr).expect("CFDictionary keys array should be non-null")
    }

    /// Copy all values into a `CFArray`.
    #[must_use]
    pub fn values(&self) -> CFArray {
        let ptr = unsafe { ffi::cf_dictionary_copy_values(self.as_ptr()) };
        CFArray::from_raw(ptr).expect("CFDictionary values array should be non-null")
    }
}

impl CFBag {
    /// Create a bag from borrowed Core Foundation values.
    #[must_use]
    pub fn from_values(values: &[&dyn AsCFType]) -> Self {
        let raw_values: Vec<*mut std::ffi::c_void> =
            values.iter().map(|value| value.as_ptr()).collect();
        let ptr = unsafe { ffi::cf_bag_create(raw_values.as_ptr(), raw_values.len()) };
        Self::from_raw(ptr).expect("CFBagCreate returned NULL")
    }

    /// Number of values in the bag.
    #[must_use]
    pub fn len(&self) -> usize {
        unsafe { ffi::cf_bag_get_count(self.as_ptr()) }
    }

    /// Whether the bag is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the bag contains `candidate`.
    #[must_use]
    pub fn contains(&self, candidate: &dyn AsCFType) -> bool {
        unsafe { ffi::cf_bag_contains_value(self.as_ptr(), candidate.as_ptr()) }
    }

    /// Number of times `candidate` appears in the bag.
    #[must_use]
    pub fn count_of_value(&self, candidate: &dyn AsCFType) -> usize {
        unsafe { ffi::cf_bag_get_count_of_value(self.as_ptr(), candidate.as_ptr()) }
    }
}

impl CFSet {
    /// Create an immutable set from borrowed Core Foundation values using type callbacks.
    #[must_use]
    pub fn from_values(values: &[&dyn AsCFType]) -> Self {
        Self::from_values_with_callbacks(values, CFSetCallbacks::Type)
    }

    /// Create an immutable set from borrowed Core Foundation values with explicit callback semantics.
    #[must_use]
    pub fn from_values_with_callbacks(values: &[&dyn AsCFType], callbacks: CFSetCallbacks) -> Self {
        let raw_values: Vec<*mut c_void> = values.iter().map(|value| value.as_ptr()).collect();
        let ptr =
            unsafe { ffi::cf_set_create(raw_values.as_ptr(), raw_values.len(), callbacks as i32) };
        Self::from_raw(ptr).expect("CFSetCreate returned NULL")
    }

    /// Create an immutable retained copy of this set.
    #[must_use]
    pub fn copy(&self) -> Self {
        let ptr = unsafe { ffi::cf_set_create_copy(self.as_ptr()) };
        Self::from_raw(ptr).expect("CFSetCreateCopy returned NULL")
    }

    /// Create a mutable retained copy of this set.
    #[must_use]
    pub fn mutable_copy(&self, capacity: usize) -> CFMutableSet {
        let ptr = unsafe { ffi::cf_set_create_mutable_copy(self.as_ptr(), capacity) };
        CFMutableSet::from_raw(ptr).expect("CFSetCreateMutableCopy returned NULL")
    }

    /// Number of values in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        unsafe { ffi::cf_set_get_count(self.as_ptr()) }
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the set contains `candidate`.
    #[must_use]
    pub fn contains(&self, candidate: &dyn AsCFType) -> bool {
        unsafe { ffi::cf_set_contains_value(self.as_ptr(), candidate.as_ptr()) }
    }

    /// Number of times `candidate` appears in the set (0 or 1).
    #[must_use]
    pub fn count_of_value(&self, candidate: &dyn AsCFType) -> usize {
        unsafe { ffi::cf_set_get_count_of_value(self.as_ptr(), candidate.as_ptr()) }
    }

    /// Copy a matching value using `CFSetGetValue` semantics.
    #[must_use]
    pub fn get(&self, candidate: &dyn AsCFType) -> Option<CFType> {
        let ptr = unsafe { ffi::cf_set_get_value(self.as_ptr(), candidate.as_ptr()) };
        CFType::from_raw(ptr)
    }

    /// Copy a matching value using `CFSetGetValueIfPresent` semantics.
    #[must_use]
    pub fn get_if_present(&self, candidate: &dyn AsCFType) -> Option<CFType> {
        let mut ptr = std::ptr::null_mut();
        let present = unsafe {
            ffi::cf_set_get_value_if_present(self.as_ptr(), candidate.as_ptr(), &mut ptr)
        };
        present.then(|| CFType::from_raw(ptr)).flatten()
    }

    /// Copy all values into a Rust vector.
    #[must_use]
    pub fn values(&self) -> Vec<CFType> {
        let len = self.len();
        let mut raw_values = vec![std::ptr::null_mut(); len];
        if !raw_values.is_empty() {
            unsafe { ffi::cf_set_get_values(self.as_ptr(), raw_values.as_mut_ptr()) };
        }
        raw_values
            .into_iter()
            .filter_map(CFType::from_raw)
            .collect()
    }

    /// Call `callback` once for each value in the set.
    pub fn for_each<F>(&self, callback: F)
    where
        F: FnMut(CFType),
    {
        let mut callback: CFSetApplyTask<'_> = Box::new(callback);
        unsafe {
            ffi::cf_set_apply_function(
                self.as_ptr(),
                std::ptr::addr_of_mut!(callback).cast::<c_void>(),
                cf_set_apply_trampoline,
            );
        }
    }
}

impl CFMutableSet {
    /// Create an empty mutable set using Core Foundation type callbacks.
    #[must_use]
    pub fn new() -> Self {
        Self::with_callbacks(0, CFSetCallbacks::Type)
    }

    /// Create an empty mutable set with explicit callback semantics.
    #[must_use]
    pub fn with_callbacks(capacity: usize, callbacks: CFSetCallbacks) -> Self {
        let ptr = unsafe { ffi::cf_set_create_mutable(capacity, callbacks as i32) };
        Self::from_raw(ptr).expect("CFSetCreateMutable returned NULL")
    }

    /// Create a mutable set from borrowed Core Foundation values using type callbacks.
    #[must_use]
    pub fn from_values(values: &[&dyn AsCFType]) -> Self {
        Self::from_values_with_callbacks(values, CFSetCallbacks::Type)
    }

    /// Create a mutable set from borrowed Core Foundation values with explicit callback semantics.
    #[must_use]
    pub fn from_values_with_callbacks(values: &[&dyn AsCFType], callbacks: CFSetCallbacks) -> Self {
        let set = Self::with_callbacks(values.len(), callbacks);
        for value in values {
            set.add(*value);
        }
        set
    }

    /// Create an immutable retained copy of this set.
    #[must_use]
    pub fn copy(&self) -> CFSet {
        let ptr = unsafe { ffi::cf_set_create_copy(self.as_ptr()) };
        CFSet::from_raw(ptr).expect("CFSetCreateCopy returned NULL")
    }

    /// Create a mutable retained copy of this set.
    #[must_use]
    pub fn mutable_copy(&self, capacity: usize) -> Self {
        let ptr = unsafe { ffi::cf_set_create_mutable_copy(self.as_ptr(), capacity) };
        Self::from_raw(ptr).expect("CFSetCreateMutableCopy returned NULL")
    }

    /// Number of values in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        unsafe { ffi::cf_set_get_count(self.as_ptr()) }
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the set contains `candidate`.
    #[must_use]
    pub fn contains(&self, candidate: &dyn AsCFType) -> bool {
        unsafe { ffi::cf_set_contains_value(self.as_ptr(), candidate.as_ptr()) }
    }

    /// Number of times `candidate` appears in the set (0 or 1).
    #[must_use]
    pub fn count_of_value(&self, candidate: &dyn AsCFType) -> usize {
        unsafe { ffi::cf_set_get_count_of_value(self.as_ptr(), candidate.as_ptr()) }
    }

    /// Copy all values into a Rust vector.
    #[must_use]
    pub fn values(&self) -> Vec<CFType> {
        let len = self.len();
        let mut raw_values = vec![std::ptr::null_mut(); len];
        if !raw_values.is_empty() {
            unsafe { ffi::cf_set_get_values(self.as_ptr(), raw_values.as_mut_ptr()) };
        }
        raw_values
            .into_iter()
            .filter_map(CFType::from_raw)
            .collect()
    }

    /// Add `candidate` if it is not already present.
    pub fn add(&self, candidate: &dyn AsCFType) {
        unsafe { ffi::cf_set_add_value(self.as_ptr(), candidate.as_ptr()) };
    }

    /// Replace the matching value if it is already present.
    pub fn replace(&self, candidate: &dyn AsCFType) {
        unsafe { ffi::cf_set_replace_value(self.as_ptr(), candidate.as_ptr()) };
    }

    /// Insert or replace `candidate`.
    pub fn set(&self, candidate: &dyn AsCFType) {
        unsafe { ffi::cf_set_set_value(self.as_ptr(), candidate.as_ptr()) };
    }

    /// Remove `candidate` if it exists.
    pub fn remove(&self, candidate: &dyn AsCFType) {
        unsafe { ffi::cf_set_remove_value(self.as_ptr(), candidate.as_ptr()) };
    }

    /// Remove every value from the set.
    pub fn clear(&self) {
        unsafe { ffi::cf_set_remove_all_values(self.as_ptr()) };
    }

    /// Call `callback` once for each value in the set.
    pub fn for_each<F>(&self, callback: F)
    where
        F: FnMut(CFType),
    {
        let mut callback: CFSetApplyTask<'_> = Box::new(callback);
        unsafe {
            ffi::cf_set_apply_function(
                self.as_ptr(),
                std::ptr::addr_of_mut!(callback).cast::<c_void>(),
                cf_set_apply_trampoline,
            );
        }
    }
}

impl Default for CFMutableSet {
    fn default() -> Self {
        Self::new()
    }
}

impl CFAttributedString {
    /// Create an attributed string with no attributes.
    #[must_use]
    pub fn new(string: &CFString) -> Self {
        let ptr = unsafe { ffi::cf_attributed_string_create(string.as_ptr()) };
        Self::from_raw(ptr).expect("CFAttributedStringCreate returned NULL")
    }

    /// Underlying plain string.
    #[must_use]
    pub fn string(&self) -> CFString {
        let ptr = unsafe { ffi::cf_attributed_string_get_string(self.as_ptr()) };
        CFString::from_raw(ptr).expect("CFAttributedStringGetString returned NULL")
    }

    /// Character length.
    #[must_use]
    pub fn len(&self) -> usize {
        unsafe { ffi::cf_attributed_string_get_length(self.as_ptr()) }
    }

    /// Whether the string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Safe wrapper around a Swift-backed `CFTree` helper.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CFTree(SwiftObject);

impl CFTree {
    /// Create a tree node with an optional Core Foundation payload.
    #[must_use]
    pub fn new(value: Option<&dyn AsCFType>) -> Self {
        let ptr =
            unsafe { ffi::cf_tree_create(value.map_or(std::ptr::null_mut(), AsCFType::as_ptr)) };
        Self(SwiftObject::from_raw(ptr).expect("tree bridge returned NULL"))
    }

    /// Wraps a +1 retained tree helper pointer and returns `None` for null.
    #[must_use]
    pub(crate) fn from_raw(ptr: *mut std::ffi::c_void) -> Option<Self> {
        SwiftObject::from_raw(ptr).map(Self)
    }

    /// Borrow the raw tree handle.
    #[must_use]
    pub(crate) const fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.0.as_ptr()
    }

    /// Append `child` to this node.
    pub fn append_child(&self, child: &Self) {
        unsafe { ffi::cf_tree_append_child(self.as_ptr(), child.as_ptr()) };
    }

    /// Number of direct children.
    #[must_use]
    pub fn child_count(&self) -> usize {
        unsafe { ffi::cf_tree_get_child_count(self.as_ptr()) }
    }

    /// Copy the child at `index`.
    #[must_use]
    pub fn child_at(&self, index: usize) -> Option<Self> {
        let ptr = unsafe { ffi::cf_tree_get_child_at_index(self.as_ptr(), index) };
        Self::from_raw(ptr)
    }

    /// Copy the payload value if present.
    #[must_use]
    pub fn value(&self) -> Option<CFType> {
        let ptr = unsafe { ffi::cf_tree_copy_value(self.as_ptr()) };
        CFType::from_raw(ptr)
    }
}

impl fmt::Debug for CFTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CFTree")
            .field("ptr", &self.as_ptr())
            .field("child_count", &self.child_count())
            .finish()
    }
}
