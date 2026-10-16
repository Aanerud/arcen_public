#![allow(warnings)]
#![allow(clippy::all)]

use super::generated::*;
use core::ptr;

#[must_use]
#[inline]
/// Apple SDK function `CFRangeMake`.
pub const fn CFRangeMake(loc: CFIndex, len: CFIndex) -> CFRange {
    CFRange {
        location: loc,
        length: len,
    }
}

/// Apple SDK constant `CFByteOrderUnknown`.
pub const CFByteOrderUnknown: CFByteOrder = 0;
/// Apple SDK constant `CFByteOrderLittleEndian`.
pub const CFByteOrderLittleEndian: CFByteOrder = 1;
/// Apple SDK constant `CFByteOrderBigEndian`.
pub const CFByteOrderBigEndian: CFByteOrder = 2;

#[must_use]
#[inline]
/// Apple SDK function `CFByteOrderGetCurrent`.
pub const fn CFByteOrderGetCurrent() -> CFByteOrder {
    if cfg!(target_endian = "little") {
        CFByteOrderLittleEndian
    } else if cfg!(target_endian = "big") {
        CFByteOrderBigEndian
    } else {
        CFByteOrderUnknown
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt16`.
pub const fn CFSwapInt16(arg: u16) -> u16 {
    arg.swap_bytes()
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt32`.
pub const fn CFSwapInt32(arg: u32) -> u32 {
    arg.swap_bytes()
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt64`.
pub const fn CFSwapInt64(arg: u64) -> u64 {
    arg.swap_bytes()
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt16BigToHost`.
pub const fn CFSwapInt16BigToHost(arg: u16) -> u16 {
    if cfg!(target_endian = "big") {
        arg
    } else {
        CFSwapInt16(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt32BigToHost`.
pub const fn CFSwapInt32BigToHost(arg: u32) -> u32 {
    if cfg!(target_endian = "big") {
        arg
    } else {
        CFSwapInt32(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt64BigToHost`.
pub const fn CFSwapInt64BigToHost(arg: u64) -> u64 {
    if cfg!(target_endian = "big") {
        arg
    } else {
        CFSwapInt64(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt16HostToBig`.
pub const fn CFSwapInt16HostToBig(arg: u16) -> u16 {
    if cfg!(target_endian = "big") {
        arg
    } else {
        CFSwapInt16(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt32HostToBig`.
pub const fn CFSwapInt32HostToBig(arg: u32) -> u32 {
    if cfg!(target_endian = "big") {
        arg
    } else {
        CFSwapInt32(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt64HostToBig`.
pub const fn CFSwapInt64HostToBig(arg: u64) -> u64 {
    if cfg!(target_endian = "big") {
        arg
    } else {
        CFSwapInt64(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt16LittleToHost`.
pub const fn CFSwapInt16LittleToHost(arg: u16) -> u16 {
    if cfg!(target_endian = "little") {
        arg
    } else {
        CFSwapInt16(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt32LittleToHost`.
pub const fn CFSwapInt32LittleToHost(arg: u32) -> u32 {
    if cfg!(target_endian = "little") {
        arg
    } else {
        CFSwapInt32(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt64LittleToHost`.
pub const fn CFSwapInt64LittleToHost(arg: u64) -> u64 {
    if cfg!(target_endian = "little") {
        arg
    } else {
        CFSwapInt64(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt16HostToLittle`.
pub const fn CFSwapInt16HostToLittle(arg: u16) -> u16 {
    if cfg!(target_endian = "little") {
        arg
    } else {
        CFSwapInt16(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt32HostToLittle`.
pub const fn CFSwapInt32HostToLittle(arg: u32) -> u32 {
    if cfg!(target_endian = "little") {
        arg
    } else {
        CFSwapInt32(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFSwapInt64HostToLittle`.
pub const fn CFSwapInt64HostToLittle(arg: u64) -> u64 {
    if cfg!(target_endian = "little") {
        arg
    } else {
        CFSwapInt64(arg)
    }
}

#[must_use]
#[inline]
/// Apple SDK function `CFConvertFloat32HostToSwapped`.
pub fn CFConvertFloat32HostToSwapped(arg: Float32) -> CFSwappedFloat32 {
    let mut bits = arg.to_bits();
    if cfg!(target_endian = "little") {
        bits = bits.swap_bytes();
    }
    CFSwappedFloat32 { v: bits }
}

#[must_use]
#[inline]
/// Apple SDK function `CFConvertFloat32SwappedToHost`.
pub fn CFConvertFloat32SwappedToHost(arg: CFSwappedFloat32) -> Float32 {
    let mut bits = arg.v;
    if cfg!(target_endian = "little") {
        bits = bits.swap_bytes();
    }
    Float32::from_bits(bits)
}

#[must_use]
#[inline]
/// Apple SDK function `CFConvertFloat64HostToSwapped`.
pub fn CFConvertFloat64HostToSwapped(arg: Float64) -> CFSwappedFloat64 {
    let mut bits = arg.to_bits();
    if cfg!(target_endian = "little") {
        bits = bits.swap_bytes();
    }
    CFSwappedFloat64 { v: bits }
}

#[must_use]
#[inline]
/// Apple SDK function `CFConvertFloat64SwappedToHost`.
pub fn CFConvertFloat64SwappedToHost(arg: CFSwappedFloat64) -> Float64 {
    let mut bits = arg.v;
    if cfg!(target_endian = "little") {
        bits = bits.swap_bytes();
    }
    Float64::from_bits(bits)
}

#[must_use]
#[inline]
/// Apple SDK function `CFConvertFloatHostToSwapped`.
pub fn CFConvertFloatHostToSwapped(arg: f32) -> CFSwappedFloat32 {
    CFConvertFloat32HostToSwapped(arg)
}

#[must_use]
#[inline]
/// Apple SDK function `CFConvertFloatSwappedToHost`.
pub fn CFConvertFloatSwappedToHost(arg: CFSwappedFloat32) -> f32 {
    CFConvertFloat32SwappedToHost(arg)
}

#[must_use]
#[inline]
/// Apple SDK function `CFConvertDoubleHostToSwapped`.
pub fn CFConvertDoubleHostToSwapped(arg: f64) -> CFSwappedFloat64 {
    CFConvertFloat64HostToSwapped(arg)
}

#[must_use]
#[inline]
/// Apple SDK function `CFConvertDoubleSwappedToHost`.
pub fn CFConvertDoubleSwappedToHost(arg: CFSwappedFloat64) -> f64 {
    CFConvertFloat64SwappedToHost(arg)
}

#[must_use]
#[inline]
/// Apple SDK function `CFUserNotificationCheckBoxChecked`.
pub const fn CFUserNotificationCheckBoxChecked(i: CFIndex) -> CFOptionFlags {
    (1_u64 << (8 + i as u32)) as CFOptionFlags
}

#[must_use]
#[inline]
/// Apple SDK function `CFUserNotificationSecureTextField`.
pub const fn CFUserNotificationSecureTextField(i: CFIndex) -> CFOptionFlags {
    (1_u64 << (16 + i as u32)) as CFOptionFlags
}

#[must_use]
#[inline]
/// Apple SDK function `CFUserNotificationPopUpSelection`.
pub const fn CFUserNotificationPopUpSelection(n: CFIndex) -> CFOptionFlags {
    ((n as u64) << 24) as CFOptionFlags
}

#[inline]
/// Initializes a `CFStringInlineBuffer` for indexed character access. Wraps `CFStringInitInlineBuffer`.
///
/// # Safety
/// `str_` must be a valid `CFStringRef`, `buf` must point to writable storage, and `range` must describe a valid slice of `str_`.
pub unsafe fn CFStringInitInlineBuffer(
    str_: CFStringRef,
    buf: *mut CFStringInlineBuffer,
    range: CFRange,
) {
    if let Some(buf) = unsafe { buf.as_mut() } {
        buf.theString = str_;
        buf.rangeToBuffer = range;
        let direct_uni = unsafe { CFStringGetCharactersPtr(str_) };
        buf.directUniCharBuffer = direct_uni;
        buf.directCStringBuffer = if direct_uni.is_null() {
            unsafe { CFStringGetCStringPtr(str_, kCFStringEncodingASCII) }
        } else {
            ptr::null()
        };
        buf.bufferedRangeStart = 0;
        buf.bufferedRangeEnd = 0;
    }
}

#[must_use]
#[inline]
/// Returns a character from a `CFStringInlineBuffer`. Wraps `CFStringGetCharacterFromInlineBuffer`.
///
/// # Safety
/// `buf` must point to an initialized inline buffer created for the referenced string, and it must remain valid for the duration of the call.
pub unsafe fn CFStringGetCharacterFromInlineBuffer(
    buf: *mut CFStringInlineBuffer,
    idx: CFIndex,
) -> UniChar {
    let Some(buf) = (unsafe { buf.as_mut() }) else {
        return 0;
    };
    if idx < 0 || idx >= buf.rangeToBuffer.length {
        return 0;
    }
    if !buf.directUniCharBuffer.is_null() {
        return unsafe {
            *buf.directUniCharBuffer
                .add((idx + buf.rangeToBuffer.location) as usize)
        };
    }
    if !buf.directCStringBuffer.is_null() {
        return unsafe {
            *buf.directCStringBuffer
                .add((idx + buf.rangeToBuffer.location) as usize) as u8
        } as UniChar;
    }
    if idx >= buf.bufferedRangeEnd || idx < buf.bufferedRangeStart {
        buf.bufferedRangeStart = (idx - 4).max(0);
        buf.bufferedRangeEnd = (buf.bufferedRangeStart + 64).min(buf.rangeToBuffer.length);
        unsafe {
            CFStringGetCharacters(
                buf.theString,
                CFRangeMake(
                    buf.rangeToBuffer.location + buf.bufferedRangeStart,
                    buf.bufferedRangeEnd - buf.bufferedRangeStart,
                ),
                buf.buffer.as_mut_ptr(),
            );
        }
    }
    buf.buffer[(idx - buf.bufferedRangeStart) as usize]
}

#[must_use]
#[inline]
/// Apple SDK function `CFStringIsSurrogateHighCharacter`.
pub const fn CFStringIsSurrogateHighCharacter(character: UniChar) -> Boolean {
    ((character >= 0xD800) && (character <= 0xDBFF)) as Boolean
}

#[must_use]
#[inline]
/// Apple SDK function `CFStringIsSurrogateLowCharacter`.
pub const fn CFStringIsSurrogateLowCharacter(character: UniChar) -> Boolean {
    ((character >= 0xDC00) && (character <= 0xDFFF)) as Boolean
}

#[must_use]
#[inline]
/// Apple SDK function `CFStringGetLongCharacterForSurrogatePair`.
pub const fn CFStringGetLongCharacterForSurrogatePair(
    surrogateHigh: UniChar,
    surrogateLow: UniChar,
) -> UTF32Char {
    (((surrogateHigh as UTF32Char - 0xD800) << 10) + (surrogateLow as UTF32Char - 0xDC00) + 0x10000)
        as UTF32Char
}

#[must_use]
#[inline]
/// Writes the UTF-16 surrogate pair for a Unicode scalar. Wraps `CFStringGetSurrogatePairForLongCharacter`.
///
/// # Safety
/// `surrogates` must be null or point to writable storage for two `UniChar` values.
pub unsafe fn CFStringGetSurrogatePairForLongCharacter(
    character: UTF32Char,
    surrogates: *mut UniChar,
) -> Boolean {
    if !(0x10000..0x110000).contains(&character) {
        return 0;
    }
    let scalar = character - 0x10000;
    if !surrogates.is_null() {
        unsafe {
            *surrogates = (0xD800 + (scalar >> 10)) as UniChar;
            *surrogates.add(1) = (0xDC00 + (scalar & 0x3ff)) as UniChar;
        }
    }
    1
}

#[must_use]
#[inline]
/// Apple SDK function `CMTagGetCategory`.
pub fn CMTagGetCategory(tag: CMTag) -> CMTagCategory {
    tag.category
}

#[must_use]
#[inline]
/// Apple SDK function `CMTagGetValue`.
pub fn CMTagGetValue(tag: CMTag) -> CMTagValue {
    tag.value
}

#[must_use]
#[inline]
/// Apple SDK function `CMTagHasCategory`.
pub fn CMTagHasCategory(tag: CMTag, category: CMTagCategory) -> Boolean {
    (CMTagGetCategory(tag) == category) as Boolean
}

#[must_use]
#[inline]
/// Apple SDK function `CMTagCategoryEqualToTagCategory`.
pub fn CMTagCategoryEqualToTagCategory(tag1: CMTag, tag2: CMTag) -> Boolean {
    (tag1.category == tag2.category) as Boolean
}

#[must_use]
#[inline]
/// Apple SDK function `CMTagIsValid`.
pub fn CMTagIsValid(tag: CMTag) -> Boolean {
    (unsafe { CMTagGetValueDataType(tag) } != kCMTagDataType_Invalid as CMTagDataType) as Boolean
}

#[must_use]
#[inline]
/// Apple SDK function `CMTagCategoryValueEqualToValue`.
pub fn CMTagCategoryValueEqualToValue(tag1: CMTag, tag2: CMTag) -> Boolean {
    ((tag1.category == tag2.category)
        && (unsafe { CMTagGetValueDataType(tag1) } == unsafe { CMTagGetValueDataType(tag2) })
        && (tag1.value == tag2.value)) as Boolean
}

#[must_use]
#[inline]
/// Creates a timebase using another timebase as its master. Wraps `CMTimebaseCreateWithMasterTimebase`.
///
/// # Safety
/// `timebaseOut` must be writable, and every supplied Core Media reference must be valid for the duration of the call.
pub unsafe fn CMTimebaseCreateWithMasterTimebase(
    allocator: CFAllocatorRef,
    masterTimebase: CMTimebaseRef,
    timebaseOut: *mut CMTimebaseRef,
) -> OSStatus {
    unsafe { CMTimebaseCreateWithSourceTimebase(allocator, masterTimebase, timebaseOut) }
}

#[must_use]
#[inline]
/// Sets the master clock for a timebase. Wraps `CMTimebaseSetMasterClock`.
///
/// # Safety
/// `timebase` and `newMasterClock` must be valid Core Media references.
pub unsafe fn CMTimebaseSetMasterClock(
    timebase: CMTimebaseRef,
    newMasterClock: CMClockRef,
) -> OSStatus {
    unsafe { CMTimebaseSetSourceClock(timebase, newMasterClock) }
}

#[must_use]
#[inline]
/// Sets the master timebase for a timebase. Wraps `CMTimebaseSetMasterTimebase`.
///
/// # Safety
/// `timebase` and `newMasterTimebase` must be valid Core Media references.
pub unsafe fn CMTimebaseSetMasterTimebase(
    timebase: CMTimebaseRef,
    newMasterTimebase: CMTimebaseRef,
) -> OSStatus {
    unsafe { CMTimebaseSetSourceTimebase(timebase, newMasterTimebase) }
}

/// Opaque tag for `CGContextRef`.
pub enum __CGContext {}

/// CoreGraphics drawing context (`CGContextRef` in CoreGraphics).
pub type CGContextRef = *mut __CGContext;

/// CoreGraphics 16-bit Unicode character code (`CGCharCode` in CoreGraphics).
pub type CGCharCode = u16;

/// CoreGraphics 16-bit virtual key code (`CGKeyCode` in CoreGraphics).
pub type CGKeyCode = u16;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
/// Opaque Apple SDK type `__CVMetalTextureCache`.
pub struct __CVMetalTextureCache {
    _unused: [u8; 0],
}

/// Apple SDK type alias `CVMetalTextureCacheRef`.
pub type CVMetalTextureCacheRef = *mut __CVMetalTextureCache;
/// Apple SDK type alias `CVMetalTextureRef`.
pub type CVMetalTextureRef = CVImageBufferRef;

extern "C" {
    /// Apple SDK function `CVMetalTextureGetTypeID`.
    pub fn CVMetalTextureGetTypeID() -> CFTypeID;
    /// Apple SDK function `CVMetalTextureGetTexture`.
    pub fn CVMetalTextureGetTexture(image: CVMetalTextureRef) -> *mut core::ffi::c_void;
    /// Apple SDK function `CVMetalTextureIsFlipped`.
    pub fn CVMetalTextureIsFlipped(image: CVMetalTextureRef) -> Boolean;
    /// Apple SDK function `CVMetalTextureGetCleanTexCoords`.
    pub fn CVMetalTextureGetCleanTexCoords(
        image: CVMetalTextureRef,
        lowerLeft: *mut f32,
        lowerRight: *mut f32,
        upperRight: *mut f32,
        upperLeft: *mut f32,
    );
    /// Apple SDK function `CVMetalTextureCacheCreateTextureFromImage`.
    pub fn CVMetalTextureCacheCreateTextureFromImage(
        allocator: CFAllocatorRef,
        textureCache: CVMetalTextureCacheRef,
        sourceImage: CVImageBufferRef,
        textureAttributes: CFDictionaryRef,
        pixelFormat: usize,
        width: usize,
        height: usize,
        planeIndex: usize,
        textureOut: *mut CVMetalTextureRef,
    ) -> CVReturn;
    /// Apple SDK function `CVMetalBufferGetBuffer`.
    pub fn CVMetalBufferGetBuffer(buffer: CVMetalBufferRef) -> *mut core::ffi::c_void;
    /// Apple SDK function `CVMetalBufferCacheCreate`.
    pub fn CVMetalBufferCacheCreate(
        allocator: CFAllocatorRef,
        cacheAttributes: CFDictionaryRef,
        metalDevice: *mut core::ffi::c_void,
        metalBufferAttributes: CFDictionaryRef,
        cacheOut: *mut CVMetalBufferCacheRef,
    ) -> CVReturn;
}

#[must_use]
#[inline]
/// Returns the process-wide Dispatch main queue pointer. Wraps `dispatch_get_main_queue`.
///
/// # Safety
/// The returned pointer follows Apple's global-queue ownership rules and must not be released as an owned +1 reference.
pub unsafe fn dispatch_get_main_queue() -> dispatch_queue_main_t {
    ptr::addr_of_mut!(_dispatch_main_q)
}
