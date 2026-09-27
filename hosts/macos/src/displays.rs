#![allow(unsafe_code)]

//! Read-only physical display inventory.
//!
//! This is intentionally not a virtual-display provider. It records the
//! current `WindowServer` inventory so later provisioning can distinguish
//! attached hardware from an unfulfilled client topology.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DisplaySnapshot {
    pub display_id: u32,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub origin_x: f64,
    pub origin_y: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DisplayProbeError {
    System(i32),
    TooManyDisplays,
    InvalidDisplay(u32),
}

const MAX_ACTIVE_DISPLAYS: usize = 32;
const MAX_ACTIVE_DISPLAYS_U32: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct InventoryCapacity {
    pub attached_displays: usize,
    pub primary_only_regions: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InventoryReport {
    pub displays: Vec<DisplaySnapshot>,
    pub capacity: InventoryCapacity,
}

impl InventoryReport {
    #[must_use]
    pub fn from_displays(displays: Vec<DisplaySnapshot>) -> Self {
        Self {
            capacity: capacity(&displays),
            displays,
        }
    }
}

impl InventoryCapacity {
    #[must_use]
    pub const fn can_serve_primary_only(self, regions: usize) -> bool {
        regions == 1 && self.primary_only_regions >= 1
    }
}

/// # Errors
///
/// Returns a system error or a bounded-inventory error when the native query
/// cannot be represented safely.
pub fn probe() -> Result<Vec<DisplaySnapshot>, DisplayProbeError> {
    #[cfg(target_os = "macos")]
    {
        probe_macos()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Vec::new())
    }
}

/// A display's rectangle in the global desktop space events are posted in:
/// origin and size in points, not pixels.
///
/// This is what pointer coordinates must be mapped onto. The capture size is
/// pixels, which differs on any scaled display, and the capture origin is
/// always zero, which is wrong for every display except the main one —
/// including a display arranged for a session, which macOS places beside the
/// real one.
#[must_use]
pub fn point_bounds(display_id: u32) -> Option<(f64, f64, f64, f64)> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `CGDisplayBounds` accepts any identifier and returns an
        // empty rectangle for one that is not active.
        let bounds = unsafe { CGDisplayBounds(display_id) };
        let usable = bounds.size.width > 0.0
            && bounds.size.height > 0.0
            && bounds.origin.x.is_finite()
            && bounds.origin.y.is_finite();
        usable.then_some((
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = display_id;
        None
    }
}

/// How far above SDR white this display can go, as a multiple of it.
///
/// 1.0 is an SDR panel; anything above is an EDR/HDR one. Read from the
/// window server rather than `NSScreen`, because an agent's main thread runs
/// the session rather than an AppKit run loop, and `NSScreen` does not learn
/// of a display added after launch until that loop runs. Measured to match
/// `NSScreen.maximumPotentialExtendedDynamicRangeColorComponentValue` on the
/// lab and on a development Mac: 16.0 for a built-in XDR panel, 5.0 for an
/// HDR virtual display, 1.0 for SDR ones. `None` when this macOS does not
/// answer, which is treated as SDR.
#[must_use]
pub fn potential_headroom(display_id: u32) -> Option<f32> {
    unsafe extern "C" {
        fn dlsym(
            handle: *mut std::ffi::c_void,
            symbol: *const std::ffi::c_char,
        ) -> *mut std::ffi::c_void;
    }
    // `RTLD_DEFAULT` on Apple platforms.
    let default = -2_isize as *mut std::ffi::c_void;
    // SAFETY: `dlsym` with RTLD_DEFAULT and a NUL-terminated name.
    let address = unsafe { dlsym(default, c"SLSDisplayGetPotentialHeadroom".as_ptr()) };
    if address.is_null() {
        return None;
    }
    // SAFETY: SkyLight's `float SLSDisplayGetPotentialHeadroom(CGDirectDisplayID)`,
    // whose return was measured against the public AppKit value above.
    let query: extern "C" fn(u32) -> f32 = unsafe { std::mem::transmute(address) };
    let headroom = query(display_id);
    headroom.is_finite().then_some(headroom)
}

/// The rectangle, in points, that spans every active display.
///
/// An absolute pointer device reports a position across the whole desktop, not
/// one display, so its coordinates are relative to this. `None` when no
/// display is active.
#[must_use]
pub fn desktop_point_bounds() -> Option<(f64, f64, f64, f64)> {
    let displays = probe().ok()?;
    let mut union: Option<(f64, f64, f64, f64)> = None;
    for display in displays {
        let Some((x, y, width, height)) = point_bounds(display.display_id) else {
            continue;
        };
        union = Some(match union {
            None => (x, y, x + width, y + height),
            Some((left, top, right, bottom)) => (
                left.min(x),
                top.min(y),
                right.max(x + width),
                bottom.max(y + height),
            ),
        });
    }
    union.map(|(left, top, right, bottom)| (left, top, right - left, bottom - top))
}

#[must_use]
pub const fn capacity(displays: &[DisplaySnapshot]) -> InventoryCapacity {
    InventoryCapacity {
        attached_displays: displays.len(),
        primary_only_regions: if displays.is_empty() { 0 } else { 1 },
    }
}

#[cfg(target_os = "macos")]
fn probe_macos() -> Result<Vec<DisplaySnapshot>, DisplayProbeError> {
    let mut count = 0_u32;
    let mut ids = [0_u32; MAX_ACTIVE_DISPLAYS];
    let error = unsafe {
        CGGetActiveDisplayList(MAX_ACTIVE_DISPLAYS_U32, ids.as_mut_ptr(), &raw mut count)
    };
    if error != 0 {
        return Err(DisplayProbeError::System(error));
    }
    let count = usize::try_from(count).map_err(|_| DisplayProbeError::TooManyDisplays)?;
    if count > ids.len() {
        return Err(DisplayProbeError::TooManyDisplays);
    }
    ids[..count]
        .iter()
        .map(|&display_id| {
            let bounds = unsafe { CGDisplayBounds(display_id) };
            let snapshot = DisplaySnapshot {
                display_id,
                pixel_width: unsafe { CGDisplayPixelsWide(display_id) },
                pixel_height: unsafe { CGDisplayPixelsHigh(display_id) },
                origin_x: bounds.origin.x,
                origin_y: bounds.origin.y,
            };
            if snapshot.is_valid() {
                Ok(snapshot)
            } else {
                Err(DisplayProbeError::InvalidDisplay(display_id))
            }
        })
        .collect()
}

impl DisplaySnapshot {
    #[must_use]
    pub fn is_valid(self) -> bool {
        self.display_id != 0
            && self.pixel_width > 0
            && self.pixel_height > 0
            && self.origin_x.is_finite()
            && self.origin_y.is_finite()
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Enumerates active displays without changing `WindowServer` state.
    ///
    /// # Safety
    ///
    /// The pointers refer to writable storage owned by this function for the
    /// duration of the call, and the capacity is bounded by that storage.
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;

    /// Reads a display's logical desktop bounds.
    ///
    /// # Safety
    ///
    /// The display identifier comes from `CGGetActiveDisplayList` and the
    /// returned value contains no borrowed pointers.
    fn CGDisplayBounds(display: u32) -> CGRect;

    /// Reads the display's pixel width.
    ///
    /// # Safety
    ///
    /// The display identifier comes from `CGGetActiveDisplayList`.
    fn CGDisplayPixelsWide(display: u32) -> usize;

    /// Reads the display's pixel height.
    ///
    /// # Safety
    ///
    /// The display identifier comes from `CGGetActiveDisplayList`.
    fn CGDisplayPixelsHigh(display: u32) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_macos_probe_is_empty_and_non_mutating() {
        #[cfg(not(target_os = "macos"))]
        assert_eq!(probe(), Ok(Vec::new()));
    }

    #[test]
    fn snapshots_are_value_types() {
        let snapshot = DisplaySnapshot {
            display_id: 1,
            pixel_width: 1920,
            pixel_height: 1080,
            origin_x: -1920.0,
            origin_y: 0.0,
        };
        assert_eq!(snapshot.origin_x, -1920.0);
    }

    #[test]
    fn inventory_report_derives_primary_only_capacity() {
        let displays = vec![DisplaySnapshot {
            display_id: 1,
            pixel_width: 1920,
            pixel_height: 1080,
            origin_x: 0.0,
            origin_y: 0.0,
        }];
        let report = InventoryReport::from_displays(displays.clone());
        assert_eq!(report.displays, displays);
        assert_eq!(
            report.capacity,
            InventoryCapacity {
                attached_displays: 1,
                primary_only_regions: 1,
            }
        );
        assert!(report.capacity.can_serve_primary_only(1));
        assert!(!report.capacity.can_serve_primary_only(2));
    }

    #[test]
    fn invalid_snapshot_is_rejected() {
        assert!(
            !DisplaySnapshot {
                display_id: 1,
                pixel_width: 0,
                pixel_height: 1080,
                origin_x: 0.0,
                origin_y: 0.0,
            }
            .is_valid()
        );
        assert!(
            !DisplaySnapshot {
                display_id: 1,
                pixel_width: 1920,
                pixel_height: 1080,
                origin_x: f64::NAN,
                origin_y: 0.0,
            }
            .is_valid()
        );
        assert!(
            DisplaySnapshot {
                display_id: 1,
                pixel_width: 1920,
                pixel_height: 1080,
                origin_x: -1920.0,
                origin_y: 0.0,
            }
            .is_valid()
        );
    }

    #[test]
    fn empty_inventory_has_no_primary_only_capacity() {
        let report = InventoryReport::from_displays(Vec::new());
        assert_eq!(report.capacity.attached_displays, 0);
        assert_eq!(report.capacity.primary_only_regions, 0);
        assert!(!report.capacity.can_serve_primary_only(1));
    }
}

/// The display modes a screen can be switched to.
///
/// Enumerated rather than assumed, because "match my primary display" can only
/// be honoured for sizes the panel will actually accept. A Deck asking for
/// 2560x1440 from a host whose display offers nothing of the sort is asking
/// for something no amount of protocol design provides.
#[cfg(target_os = "macos")]
#[must_use]
pub fn available_modes(display_id: u32) -> Vec<(usize, usize)> {
    // Asking with no options hides every scaled and duplicate-resolution mode,
    // which is most of what a modern display offers. The answer to "what can
    // this screen do" was one mode until this dictionary was passed.
    let key = c"kCGDisplayShowDuplicateLowResolutionModes";
    // SAFETY: the key is a static C string and the value a process-lifetime
    // Core Foundation singleton; the dictionary is created with +1 and
    // released below.
    let options = unsafe {
        let key = CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), 0x0800_0100);
        let mut keys = [key.cast::<std::ffi::c_void>()];
        let mut values = [kCFBooleanTrue];
        let dictionary = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            std::ptr::null(),
            std::ptr::null(),
        );
        crate::input::release_core_foundation(key.cast());
        dictionary
    };
    // SAFETY: both calls take a display id and return either a retained array
    // or null; nothing is borrowed from the caller.
    let modes = unsafe { CGDisplayCopyAllDisplayModes(display_id, options) };
    if !options.is_null() {
        // SAFETY: created with +1 above.
        unsafe { crate::input::release_core_foundation(options.cast()) };
    }
    if modes.is_null() {
        return Vec::new();
    }
    // SAFETY: `modes` is the live array returned above.
    let count = unsafe { CFArrayGetCount(modes) };
    let mut sizes = Vec::new();
    for index in 0..count {
        // SAFETY: `index` is in bounds by construction.
        let mode = unsafe { CFArrayGetValueAtIndex(modes, index) };
        if mode.is_null() {
            continue;
        }
        // SAFETY: each element is a CGDisplayMode.
        let (width, height) = unsafe {
            (
                CGDisplayModeGetPixelWidth(mode),
                CGDisplayModeGetPixelHeight(mode),
            )
        };
        if width > 0 && height > 0 && !sizes.contains(&(width, height)) {
            sizes.push((width, height));
        }
    }
    // SAFETY: the array was returned with +1 and is released here.
    unsafe { crate::input::release_core_foundation(modes.cast()) };
    sizes.sort_unstable();
    sizes
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCopyAllDisplayModes(
        display: u32,
        options: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CGDisplayModeGetPixelWidth(mode: *const std::ffi::c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *const std::ffi::c_void) -> usize;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: *const std::ffi::c_void) -> isize;
    fn CFArrayGetValueAtIndex(
        array: *const std::ffi::c_void,
        index: isize,
    ) -> *const std::ffi::c_void;
    fn CFStringCreateWithCString(
        allocator: *const std::ffi::c_void,
        bytes: *const std::ffi::c_char,
        encoding: u32,
    ) -> *const std::ffi::c_void;
    fn CFDictionaryCreate(
        allocator: *const std::ffi::c_void,
        keys: *mut *const std::ffi::c_void,
        values: *mut *const std::ffi::c_void,
        count: isize,
        key_callbacks: *const std::ffi::c_void,
        value_callbacks: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    static kCFBooleanTrue: *const std::ffi::c_void;
}
