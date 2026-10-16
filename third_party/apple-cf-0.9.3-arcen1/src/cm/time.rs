#![allow(clippy::missing_panics_doc)]

//! Core Media time types

use std::ffi::c_void;
use std::fmt;

/// `CMTime` representation matching Core Media's `CMTime`
///
/// Represents a rational time value with a 64-bit numerator and 32-bit denominator.
///
/// # Examples
///
/// ```
/// use apple_cf::cm::CMTime;
///
/// // Create a time of 1 second (30/30)
/// let time = CMTime::new(30, 30);
/// assert_eq!(time.as_seconds(), Some(1.0));
///
/// // Create a time of 2.5 seconds at 1000 Hz timescale
/// let time = CMTime::new(2500, 1000);
/// assert_eq!(time.value, 2500);
/// assert_eq!(time.timescale, 1000);
/// assert_eq!(time.as_seconds(), Some(2.5));
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

impl std::hash::Hash for CMTime {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
        self.timescale.hash(state);
        self.flags.hash(state);
        self.epoch.hash(state);
    }
}

/// Sample timing information
///
/// Contains timing data for a media sample (audio or video frame).
///
/// # Examples
///
/// ```
/// use apple_cf::cm::{CMSampleTimingInfo, CMTime};
///
/// let timing = CMSampleTimingInfo::new();
/// assert!(!timing.is_valid());
///
/// let duration = CMTime::new(1, 30);
/// let pts = CMTime::new(100, 30);
/// let dts = CMTime::new(100, 30);
/// let timing = CMSampleTimingInfo::with_times(duration, pts, dts);
/// assert!(timing.is_valid());
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CMSampleTimingInfo {
    pub duration: CMTime,
    pub presentation_time_stamp: CMTime,
    pub decode_time_stamp: CMTime,
}

impl std::hash::Hash for CMSampleTimingInfo {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.duration.hash(state);
        self.presentation_time_stamp.hash(state);
        self.decode_time_stamp.hash(state);
    }
}

impl CMSampleTimingInfo {
    /// Create a new timing info with all times set to invalid
    ///
    /// # Examples
    ///
    /// ```
    /// use apple_cf::cm::CMSampleTimingInfo;
    ///
    /// let timing = CMSampleTimingInfo::new();
    /// assert!(!timing.is_valid());
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            duration: CMTime::INVALID,
            presentation_time_stamp: CMTime::INVALID,
            decode_time_stamp: CMTime::INVALID,
        }
    }

    /// Create timing info with specific values
    #[must_use]
    pub const fn with_times(
        duration: CMTime,
        presentation_time_stamp: CMTime,
        decode_time_stamp: CMTime,
    ) -> Self {
        Self {
            duration,
            presentation_time_stamp,
            decode_time_stamp,
        }
    }

    /// Check if all timing fields are valid
    /// Returns whether this time carries Core Media's valid flag.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.duration.is_valid()
            && self.presentation_time_stamp.is_valid()
            && self.decode_time_stamp.is_valid()
    }

    /// Check if presentation timestamp is valid
    #[must_use]
    pub const fn has_valid_presentation_time(&self) -> bool {
        self.presentation_time_stamp.is_valid()
    }

    /// Check if decode timestamp is valid
    #[must_use]
    pub const fn has_valid_decode_time(&self) -> bool {
        self.decode_time_stamp.is_valid()
    }

    /// Check if duration is valid
    #[must_use]
    pub const fn has_valid_duration(&self) -> bool {
        self.duration.is_valid()
    }

    /// Get the presentation timestamp in seconds
    #[must_use]
    pub fn presentation_seconds(&self) -> Option<f64> {
        self.presentation_time_stamp.as_seconds()
    }

    /// Get the decode timestamp in seconds
    #[must_use]
    pub fn decode_seconds(&self) -> Option<f64> {
        self.decode_time_stamp.as_seconds()
    }

    /// Get the duration in seconds
    #[must_use]
    pub fn duration_seconds(&self) -> Option<f64> {
        self.duration.as_seconds()
    }
}

impl Default for CMSampleTimingInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CMSampleTimingInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CMSampleTimingInfo(pts: {}, dts: {}, duration: {})",
            self.presentation_time_stamp, self.decode_time_stamp, self.duration
        )
    }
}

impl CMTime {
    /// Core Media's zero time value (`kCMTimeZero`).
    pub const ZERO: Self = Self {
        value: 0,
        timescale: 0,
        flags: 1,
        epoch: 0,
    };

    /// Core Media's invalid time sentinel (`kCMTimeInvalid`).
    pub const INVALID: Self = Self {
        value: 0,
        timescale: 0,
        flags: 0,
        epoch: 0,
    };

    /// Creates a valid `CMTime` with the supplied value and timescale.
    #[must_use]
    pub const fn new(value: i64, timescale: i32) -> Self {
        Self {
            value,
            timescale,
            flags: 1,
            epoch: 0,
        }
    }

    /// Returns whether this time carries Core Media's valid flag.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.flags & 0x1 != 0
    }

    /// Check if this time represents zero
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.value == 0 && self.is_valid()
    }

    /// Check if this time is indefinite
    #[must_use]
    pub const fn is_indefinite(&self) -> bool {
        self.flags & 0x2 != 0
    }

    /// Check if this time is positive infinity
    #[must_use]
    pub const fn is_positive_infinity(&self) -> bool {
        self.flags & 0x4 != 0
    }

    /// Check if this time is negative infinity
    #[must_use]
    pub const fn is_negative_infinity(&self) -> bool {
        self.flags & 0x8 != 0
    }

    /// Check if this time has been rounded
    #[must_use]
    pub const fn has_been_rounded(&self) -> bool {
        self.flags & 0x10 != 0
    }

    /// Compare two times for equality (value and timescale)
    #[must_use]
    pub const fn equals(&self, other: &Self) -> bool {
        if !self.is_valid() || !other.is_valid() {
            return false;
        }
        self.value == other.value && self.timescale == other.timescale
    }

    /// Create a time representing positive infinity
    #[must_use]
    pub const fn positive_infinity() -> Self {
        Self {
            value: 0,
            timescale: 0,
            flags: 0x5, // kCMTimeFlags_Valid | kCMTimeFlags_PositiveInfinity
            epoch: 0,
        }
    }

    /// Create a time representing negative infinity
    #[must_use]
    pub const fn negative_infinity() -> Self {
        Self {
            value: 0,
            timescale: 0,
            flags: 0x9, // kCMTimeFlags_Valid | kCMTimeFlags_NegativeInfinity
            epoch: 0,
        }
    }

    /// Create an indefinite time
    #[must_use]
    pub const fn indefinite() -> Self {
        Self {
            value: 0,
            timescale: 0,
            flags: 0x3, // kCMTimeFlags_Valid | kCMTimeFlags_Indefinite
            epoch: 0,
        }
    }

    /// Converts this time to seconds when it is valid and has a non-zero timescale.
    #[must_use]
    pub fn as_seconds(&self) -> Option<f64> {
        if self.is_valid() && self.timescale != 0 {
            // Precision loss is acceptable for time conversion to seconds
            #[allow(clippy::cast_precision_loss)]
            Some(self.value as f64 / f64::from(self.timescale))
        } else {
            None
        }
    }

    /// Construct a `CMTime` from a floating-point number of seconds
    /// with the requested `preferred_timescale` (typically `600` for
    /// video, `48000` / `44100` for audio). Wraps `CMTimeMakeWithSeconds`.
    #[must_use]
    pub fn from_seconds(seconds: f64, preferred_timescale: i32) -> Self {
        extern "C" {
            fn CMTimeMakeWithSeconds(seconds: f64, preferredTimescale: i32) -> CMTime;
        }
        unsafe { CMTimeMakeWithSeconds(seconds, preferred_timescale) }
    }

    /// Add two times. Wraps `CMTimeAdd`. Returns
    /// [`CMTime::INVALID`] if either operand is invalid.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, other: Self) -> Self {
        extern "C" {
            fn CMTimeAdd(addend1: CMTime, addend2: CMTime) -> CMTime;
        }
        unsafe { CMTimeAdd(self, other) }
    }

    /// Subtract `other` from `self`. Wraps `CMTimeSubtract`.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn subtract(self, other: Self) -> Self {
        extern "C" {
            fn CMTimeSubtract(minuend: CMTime, subtrahend: CMTime) -> CMTime;
        }
        unsafe { CMTimeSubtract(self, other) }
    }

    /// Multiply by an integer. Wraps `CMTimeMultiply`.
    #[must_use]
    pub fn multiply(self, multiplier: i32) -> Self {
        extern "C" {
            fn CMTimeMultiply(time: CMTime, multiplier: i32) -> CMTime;
        }
        unsafe { CMTimeMultiply(self, multiplier) }
    }

    /// Multiply by an `f64` factor. Wraps `CMTimeMultiplyByFloat64`.
    #[must_use]
    pub fn multiply_by_f64(self, factor: f64) -> Self {
        extern "C" {
            fn CMTimeMultiplyByFloat64(time: CMTime, multiplier: f64) -> CMTime;
        }
        unsafe { CMTimeMultiplyByFloat64(self, factor) }
    }

    /// Compare two times. Returns `Ordering::Less` if `self < other`,
    /// `Greater` if `self > other`, `Equal` otherwise. Wraps
    /// `CMTimeCompare`.
    #[must_use]
    pub fn compare(self, other: Self) -> core::cmp::Ordering {
        extern "C" {
            fn CMTimeCompare(time1: CMTime, time2: CMTime) -> i32;
        }
        let c = unsafe { CMTimeCompare(self, other) };
        c.cmp(&0)
    }

    /// Convert this time to a different `new_timescale`, applying
    /// Apple's default rounding (`kCMTimeRoundingMethod_Default`).
    /// Wraps `CMTimeConvertScale`.
    #[must_use]
    pub fn convert_scale(self, new_timescale: i32) -> Self {
        extern "C" {
            fn CMTimeConvertScale(time: CMTime, newTimescale: i32, method: u32) -> CMTime;
        }
        unsafe { CMTimeConvertScale(self, new_timescale, 0) }
    }
}

impl Default for CMTime {
    fn default() -> Self {
        Self::INVALID
    }
}

impl fmt::Display for CMTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(seconds) = self.as_seconds() {
            write!(f, "{seconds:.3}s")
        } else {
            write!(f, "invalid")
        }
    }
}

/// `CMTimeRange` representation matching Core Media's `CMTimeRange`.
///
/// ```
/// use apple_cf::cm::{CMTime, CMTimeRange};
///
/// let range = CMTimeRange::new(CMTime::new(0, 600), CMTime::new(300, 600));
/// assert_eq!(range.end(), CMTime::new(300, 600));
/// assert!(range.contains_time(CMTime::new(150, 600)));
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CMTimeRange {
    pub start: CMTime,
    pub duration: CMTime,
}

impl CMTimeRange {
    /// Core Media's invalid time-range sentinel (`kCMTimeRangeInvalid`).
    pub const INVALID: Self = Self {
        start: CMTime::INVALID,
        duration: CMTime::INVALID,
    };

    /// Creates a Core Media time range from a start time and duration.
    #[must_use]
    pub const fn new(start: CMTime, duration: CMTime) -> Self {
        Self { start, duration }
    }

    /// Returns the range end time via `CMTimeRangeGetEnd`.
    #[must_use]
    pub fn end(&self) -> CMTime {
        extern "C" {
            fn CMTimeRangeGetEnd(range: CMTimeRange) -> CMTime;
        }
        unsafe { CMTimeRangeGetEnd(*self) }
    }

    /// Returns whether both the start and duration are valid `CMTime` values.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.start.is_valid() && self.duration.is_valid()
    }

    /// Returns whether this range contains the supplied `CMTime`.
    #[must_use]
    pub fn contains_time(&self, time: CMTime) -> bool {
        extern "C" {
            fn CMTimeRangeContainsTime(range: CMTimeRange, time: CMTime) -> bool;
        }
        unsafe { CMTimeRangeContainsTime(*self, time) }
    }

    /// Returns whether this range fully contains `other`.
    #[must_use]
    pub fn contains_range(&self, other: Self) -> bool {
        extern "C" {
            fn CMTimeRangeContainsTimeRange(range: CMTimeRange, otherRange: CMTimeRange) -> bool;
        }
        unsafe { CMTimeRangeContainsTimeRange(*self, other) }
    }

    /// Returns the intersection of this range and `other`.
    #[must_use]
    pub fn intersection(&self, other: Self) -> Self {
        extern "C" {
            fn CMTimeRangeGetIntersection(
                range: CMTimeRange,
                otherRange: CMTimeRange,
            ) -> CMTimeRange;
        }
        unsafe { CMTimeRangeGetIntersection(*self, other) }
    }

    /// Returns the union of this range and `other`.
    #[must_use]
    pub fn union(&self, other: Self) -> Self {
        extern "C" {
            fn CMTimeRangeGetUnion(range: CMTimeRange, otherRange: CMTimeRange) -> CMTimeRange;
        }
        unsafe { CMTimeRangeGetUnion(*self, other) }
    }
}

impl fmt::Display for CMTimeRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CMTimeRange(start: {}, duration: {})",
            self.start, self.duration
        )
    }
}

/// `CMClock` wrapper for synchronization clock
///
/// Represents a Core Media clock used for time synchronization.
/// Available on macOS 13.0+.
pub struct CMClock {
    ptr: *const c_void,
}

impl PartialEq for CMClock {
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl Eq for CMClock {}

impl std::hash::Hash for CMClock {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ptr.hash(state);
    }
}

impl CMClock {
    /// Wraps a +1 retained `CMClockRef` and returns `None` for null.
    #[must_use]
    pub fn from_raw(ptr: *const c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr })
        }
    }

    /// Host-time master clock.
    #[must_use]
    pub fn host_time_clock() -> Self {
        extern "C" {
            fn CMClockGetHostTimeClock() -> *const c_void;
            fn CFRetain(cf: *const c_void) -> *const c_void;
        }
        let ptr = unsafe { CMClockGetHostTimeClock() };
        assert!(!ptr.is_null(), "CMClockGetHostTimeClock returned NULL");
        let retained = unsafe { CFRetain(ptr) };
        Self { ptr: retained }
    }

    /// Wraps a raw `CMClockRef` by taking ownership without retaining it.
    ///
    /// # Safety
    /// The caller must ensure `ptr` is a valid +1 retained `CMClockRef`.
    #[allow(dead_code)]
    pub(crate) const fn from_ptr(ptr: *const c_void) -> Self {
        Self { ptr }
    }

    /// Returns the raw pointer to the underlying `CMClock`
    #[must_use]
    pub const fn as_ptr(&self) -> *const c_void {
        self.ptr
    }

    /// Get the current time from this clock
    ///
    /// Note: Returns invalid time. Use `as_ptr()` with Core Media APIs directly
    /// for full clock functionality.
    #[must_use]
    pub const fn time(&self) -> CMTime {
        // This would require FFI to CMClockGetTime - for now return invalid
        // Users can use the pointer directly with Core Media APIs
        CMTime::INVALID
    }
}

impl Drop for CMClock {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // CMClock is a CFType, needs CFRelease
            extern "C" {
                fn CFRelease(cf: *const c_void);
            }
            unsafe {
                CFRelease(self.ptr);
            }
        }
    }
}

impl Clone for CMClock {
    fn clone(&self) -> Self {
        if self.ptr.is_null() {
            Self {
                ptr: std::ptr::null(),
            }
        } else {
            extern "C" {
                fn CFRetain(cf: *const c_void) -> *const c_void;
            }
            unsafe {
                Self {
                    ptr: CFRetain(self.ptr),
                }
            }
        }
    }
}

impl std::fmt::Debug for CMClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CMClock").field("ptr", &self.ptr).finish()
    }
}

impl fmt::Display for CMClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ptr.is_null() {
            write!(f, "CMClock(null)")
        } else {
            write!(f, "CMClock({:p})", self.ptr)
        }
    }
}

// SAFETY: `CMClockRef` is a Core Foundation type documented by Apple as
// thread-safe; time queries are read-only operations on an opaque pointer.
unsafe impl Send for CMClock {}
unsafe impl Sync for CMClock {}
