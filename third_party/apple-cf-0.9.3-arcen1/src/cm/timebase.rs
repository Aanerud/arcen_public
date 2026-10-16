//! `CMTimebase` wrapper.
//!
#![allow(clippy::missing_errors_doc)]

//! ```rust,no_run
//! use apple_cf::cm::{CMClock, CMTime, CMTimebase};
//!
//! let clock = CMClock::host_time_clock();
//! let timebase = CMTimebase::with_master_clock(&clock).expect("timebase");
//! assert!(timebase.time().is_valid());
//! assert_eq!(timebase.set_rate(1.0), 0);
//! assert_eq!(timebase.set_time(CMTime::new(0, 600)), 0);
//! ```

use super::{CMClock, CMTime};
use std::ffi::c_void;
use std::fmt;

/// Owned wrapper around `CMTimebaseRef`.
pub struct CMTimebase {
    ptr: *const c_void,
}

impl CMTimebase {
    /// Create a timebase that uses `master_clock` as its time source.
    pub fn with_master_clock(master_clock: &CMClock) -> Result<Self, i32> {
        extern "C" {
            fn CMTimebaseCreateWithMasterClock(
                allocator: *const c_void,
                masterClock: *const c_void,
                timebaseOut: *mut *const c_void,
            ) -> i32;
        }
        let mut ptr = std::ptr::null();
        let status = unsafe {
            CMTimebaseCreateWithMasterClock(std::ptr::null(), master_clock.as_ptr(), &mut ptr)
        };
        if status == 0 && !ptr.is_null() {
            Ok(Self { ptr })
        } else {
            Err(status)
        }
    }

    /// Wraps a +1 retained `CMTimebaseRef` and returns `None` for null.
    #[must_use]
    pub fn from_raw(ptr: *const c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr })
        }
    }

    /// Borrow the underlying `CMTimebaseRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> *const c_void {
        self.ptr
    }

    /// Current time.
    #[must_use]
    pub fn time(&self) -> CMTime {
        extern "C" {
            fn CMTimebaseGetTime(timebase: *const c_void) -> CMTime;
        }
        unsafe { CMTimebaseGetTime(self.ptr) }
    }

    /// Set the current time.
    #[must_use]
    pub fn set_time(&self, time: CMTime) -> i32 {
        extern "C" {
            fn CMTimebaseSetTime(timebase: *const c_void, time: CMTime) -> i32;
        }
        unsafe { CMTimebaseSetTime(self.ptr, time) }
    }

    /// Playback rate relative to the master clock.
    #[must_use]
    pub fn rate(&self) -> f64 {
        extern "C" {
            fn CMTimebaseGetRate(timebase: *const c_void) -> f64;
        }
        unsafe { CMTimebaseGetRate(self.ptr) }
    }

    /// Set the playback rate relative to the master clock.
    #[must_use]
    pub fn set_rate(&self, rate: f64) -> i32 {
        extern "C" {
            fn CMTimebaseSetRate(timebase: *const c_void, rate: f64) -> i32;
        }
        unsafe { CMTimebaseSetRate(self.ptr, rate) }
    }

    /// Copy the master clock.
    #[must_use]
    pub fn master_clock(&self) -> Option<CMClock> {
        extern "C" {
            fn CMTimebaseCopyMasterClock(timebase: *const c_void) -> *const c_void;
        }
        let ptr = unsafe { CMTimebaseCopyMasterClock(self.ptr) };
        CMClock::from_raw(ptr)
    }
}

impl Clone for CMTimebase {
    fn clone(&self) -> Self {
        extern "C" {
            fn CFRetain(cf: *const c_void) -> *const c_void;
        }
        let ptr = unsafe { CFRetain(self.ptr) };
        Self { ptr }
    }
}

impl Drop for CMTimebase {
    fn drop(&mut self) {
        extern "C" {
            fn CFRelease(cf: *const c_void);
        }
        unsafe { CFRelease(self.ptr) };
    }
}

impl PartialEq for CMTimebase {
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl Eq for CMTimebase {}

impl std::hash::Hash for CMTimebase {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ptr.hash(state);
    }
}

impl fmt::Debug for CMTimebase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CMTimebase")
            .field("ptr", &self.ptr)
            .field("time", &self.time())
            .field("rate", &self.rate())
            .finish()
    }
}

// SAFETY: `CMTimebaseRef` is a Core Foundation type; Apple documents its
// retain/release as thread-safe.  The wrapper only holds the opaque pointer and
// delegates all mutations through the Core Media API.
unsafe impl Send for CMTimebase {}
unsafe impl Sync for CMTimebase {}
