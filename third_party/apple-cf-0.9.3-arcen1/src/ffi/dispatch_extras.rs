#![allow(missing_docs)]

use core::ffi::c_void;

extern "C" {
    /// Swift bridge function `acf_dispatch_async_f` for the corresponding Apple API.
    pub fn acf_dispatch_async_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    /// Swift bridge function `acf_dispatch_async_and_wait_f` for the corresponding Apple API.
    pub fn acf_dispatch_async_and_wait_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    /// Swift bridge function `acf_dispatch_apply_f` for the corresponding Apple API.
    pub fn acf_dispatch_apply_f(
        iterations: usize,
        queue: *mut c_void,
        context: *mut c_void,
        work: extern "C" fn(usize, *mut c_void),
    );

    /// Swift bridge function `acf_dispatch_group_create` for the corresponding Apple API.
    pub fn acf_dispatch_group_create() -> *mut c_void;
    /// Swift bridge function `acf_dispatch_group_enter` for the corresponding Apple API.
    pub fn acf_dispatch_group_enter(group: *mut c_void);
    /// Swift bridge function `acf_dispatch_group_leave` for the corresponding Apple API.
    pub fn acf_dispatch_group_leave(group: *mut c_void);
    /// Swift bridge function `acf_dispatch_group_wait` for the corresponding Apple API.
    pub fn acf_dispatch_group_wait(group: *mut c_void, timeout_ms: i64) -> bool;

    /// Swift bridge function `acf_dispatch_semaphore_create` for the corresponding Apple API.
    pub fn acf_dispatch_semaphore_create(value: i64) -> *mut c_void;
    /// Swift bridge function `acf_dispatch_semaphore_signal` for the corresponding Apple API.
    pub fn acf_dispatch_semaphore_signal(semaphore: *mut c_void) -> i64;
    /// Swift bridge function `acf_dispatch_semaphore_wait` for the corresponding Apple API.
    pub fn acf_dispatch_semaphore_wait(semaphore: *mut c_void, timeout_ms: i64) -> bool;

    /// Swift bridge function `acf_dispatch_source_timer_create` for the corresponding Apple API.
    pub fn acf_dispatch_source_timer_create(interval_ms: u64, leeway_ms: u64) -> *mut c_void;
    /// Swift bridge function `acf_dispatch_source_timer_resume` for the corresponding Apple API.
    pub fn acf_dispatch_source_timer_resume(source: *mut c_void);
    /// Swift bridge function `acf_dispatch_source_timer_cancel` for the corresponding Apple API.
    pub fn acf_dispatch_source_timer_cancel(source: *mut c_void);
    /// Swift bridge function `acf_dispatch_source_timer_fire_count` for the corresponding Apple API.
    pub fn acf_dispatch_source_timer_fire_count(source: *mut c_void) -> u64;
}
