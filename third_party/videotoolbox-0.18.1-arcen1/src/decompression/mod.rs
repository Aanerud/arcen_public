//! [`DecompressionSession`] — hardware H.264/HEVC/ProRes decoder.
//!
//! Mirror of [`crate::compression::CompressionSession`] for the decoder
//! direction. Feed `CMSampleBuffer`s in via [`DecompressionSession::decode`],
//! receive `CVPixelBuffer`s out via the callback you registered at construction.

use core::ffi::c_void;
use core::ptr;
use std::sync::{Arc, Mutex};

use apple_cf::cf::{AsCFType, CFDictionary, CFType};
#[cfg(feature = "async")]
use doom_fish_utils::completion::{AsyncCompletion, SyncCompletionPtr};
#[cfg(feature = "async")]
use doom_fish_utils::panic_safe::catch_user_panic;

use crate::error::VTError;
use crate::ffi;
use crate::session::{self, Codec};
use crate::tagged_buffer_group::TaggedBufferGroup;

/// One decoded video frame.
pub struct DecodedFrame {
    /// Underlying `CVImageBuffer` (typically a `CVPixelBuffer`) — `None`
    /// when the decoder dropped or skipped a frame.
    pub image_buffer: Option<apple_cf::cv::CVPixelBuffer>,
    /// Presentation timestamp of the source sample buffer.
    pub presentation_time: (i64, i32),
    /// Presentation duration of the decoded frame.
    pub duration: (i64, i32),
    /// Decoder hint flags (asynchronous / image-buffer-modifiable / etc.).
    pub info_flags: ffi::VTDecodeInfoFlags,
    /// Apple's decoder status code. 0 indicates success.
    pub status: i32,
}

/// One decoded multi-image frame.
pub struct DecodedMultiImageFrame {
    /// Multi-image output (for example left/right eye pixel buffers).
    pub tagged_buffer_group: Option<TaggedBufferGroup>,
    /// Presentation timestamp of the source sample buffer.
    pub presentation_time: (i64, i32),
    /// Presentation duration of the decoded frame.
    pub duration: (i64, i32),
    /// Decoder hint flags (asynchronous / image-buffer-modifiable / etc.).
    pub info_flags: ffi::VTDecodeInfoFlags,
    /// Apple's decoder status code. 0 indicates success.
    pub status: i32,
}

type DecodeCallback = Box<dyn FnMut(DecodedFrame) + Send + 'static>;
type MultiImageDecodeCallback = Box<dyn FnMut(DecodedMultiImageFrame) + Send + 'static>;

struct CallbackState {
    callback: Mutex<DecodeCallback>,
    multi_image_callback: Mutex<Option<MultiImageDecodeCallback>>,
}

#[cfg(feature = "async")]
struct AsyncDecodeContext(SyncCompletionPtr);

/// Hardware video decompression session.
///
/// Build with [`DecompressionSession::new`] passing a format description
/// (from the encoder side, or extracted from your input file). Decoded
/// frames flow through the callback you supply.
///
/// # Lifetime / threading
///
/// Drop the session to invalidate it. Decoder callbacks fire on Apple's
/// internal queue — they may run after `decode()` returns and before
/// `wait_for_async_frames()`.
pub struct DecompressionSession {
    session: ffi::VTDecompressionSessionRef,
    state: Arc<CallbackState>,
}

unsafe impl Send for DecompressionSession {}
unsafe impl Sync for DecompressionSession {}

crate::utils::retained::vt_retained!(
    DecompressionSession,
    field = session,
    invalidate = ffi::VTDecompressionSessionInvalidate,
    release = ffi::CFRelease,
);

/// `unimpErr`: the running OS does not provide the requested routine.
const UNIMPLEMENTED_STATUS: ffi::OSStatus = -4;

type DecodeFrameWithOptionsFn = unsafe extern "C" fn(
    ffi::VTDecompressionSessionRef,
    ffi::CMSampleBufferRef,
    ffi::VTDecodeFrameFlags,
    *const std::ffi::c_void,
    *mut std::ffi::c_void,
    *mut ffi::VTDecodeInfoFlags,
) -> ffi::OSStatus;

/// Look up `VTDecompressionSessionDecodeFrameWithOptions` in the loaded
/// VideoToolbox image, once per process.
fn decode_frame_with_options_fn() -> Option<DecodeFrameWithOptionsFn> {
    use std::sync::OnceLock;

    extern "C" {
        fn dlsym(
            handle: *mut std::ffi::c_void,
            symbol: *const std::ffi::c_char,
        ) -> *mut std::ffi::c_void;
    }
    // `RTLD_DEFAULT` on Apple platforms.
    const RTLD_DEFAULT: *mut std::ffi::c_void = -2_isize as *mut std::ffi::c_void;

    static RESOLVED: OnceLock<usize> = OnceLock::new();
    let address = *RESOLVED.get_or_init(|| {
        // SAFETY: the name is a NUL-terminated C string literal and
        // RTLD_DEFAULT searches the images already loaded by this process.
        let symbol = unsafe {
            dlsym(RTLD_DEFAULT, c"VTDecompressionSessionDecodeFrameWithOptions".as_ptr())
        };
        symbol as usize
    });
    if address == 0 {
        None
    } else {
        // SAFETY: the resolved symbol has exactly the C signature declared
        // by VideoToolbox's `VTDecompressionSessionDecodeFrameWithOptions`.
        Some(unsafe { std::mem::transmute::<usize, DecodeFrameWithOptionsFn>(address) })
    }
}

impl DecompressionSession {
    /// CoreFoundation type identifier for `VTDecompressionSession`.
    #[must_use]
    pub fn type_id() -> usize {
        // SAFETY: `VTDecompressionSessionGetTypeID` is a standard Apple SDK function
        // that returns a static type ID. Safe to call from any thread.
        unsafe { ffi::VTDecompressionSessionGetTypeID() }
    }

    /// Returns `true` when the current machine advertises hardware decode for
    /// the given codec family.
    #[must_use]
    pub fn is_hardware_decode_supported(codec: Codec) -> bool {
        // SAFETY: `VTIsHardwareDecodeSupported` is a standard query function
        // that performs no I/O and has no side effects.
        unsafe { ffi::VTIsHardwareDecodeSupported(codec.as_cm_codec_type()) != 0 }
    }

    /// Returns `true` when the current machine advertises stereo MV-HEVC
    /// decode support.
    #[must_use]
    pub fn is_stereo_mvhevc_decode_supported() -> bool {
        // SAFETY: `VTIsStereoMVHEVCDecodeSupported` is a standard query function
        // that performs no I/O and has no side effects.
        unsafe { ffi::VTIsStereoMVHEVCDecodeSupported() != 0 }
    }

    /// Open a decompression session for the given format description.
    /// `format_description` is the `CMFormatDescriptionRef` Apple expects
    /// — typically obtained from `CMSampleBuffer.format_description()` on
    /// the first sample of the stream.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncoderCallback`] wrapping the raw `OSStatus`
    /// if `VTDecompressionSessionCreate` fails.
    pub fn new<F>(
        format_description: &apple_cf::cm::CMFormatDescription,
        callback: F,
    ) -> Result<Self, VTError>
    where
        F: FnMut(DecodedFrame) + Send + 'static,
    {
        Self::new_with_image_buffer_attributes(format_description, None, callback)
    }

    /// Like [`new`](Self::new) but lets you specify the decoder's destination
    /// image-buffer attributes (the `destinationImageBufferAttributes` argument
    /// to `VTDecompressionSessionCreate`).
    ///
    /// Use this to force the output pixel format (for example a `420f`
    /// full-range NV12 buffer required by some renderers) or to require
    /// IOSurface backing for zero-copy GPU interop, by passing a dictionary
    /// keyed on `kCVPixelBufferPixelFormatTypeKey` /
    /// `kCVPixelBufferIOSurfacePropertiesKey`.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncoderCallback`] wrapping the raw `OSStatus`
    /// if `VTDecompressionSessionCreate` fails.
    pub fn new_with_image_buffer_attributes<F>(
        format_description: &apple_cf::cm::CMFormatDescription,
        image_buffer_attributes: Option<&CFDictionary>,
        callback: F,
    ) -> Result<Self, VTError>
    where
        F: FnMut(DecodedFrame) + Send + 'static,
    {
        let state = Arc::new(CallbackState {
            callback: Mutex::new(Box::new(callback)),
            multi_image_callback: Mutex::new(None),
        });
        let state_for_callback = state.clone();
        let ref_con = Arc::into_raw(state_for_callback)
            .cast::<c_void>()
            .cast_mut();

        let record = ffi::VTDecompressionOutputCallbackRecord {
            decompression_output_callback: decode_trampoline,
            decompression_output_ref_con: ref_con,
        };

        let mut session: ffi::VTDecompressionSessionRef = ptr::null_mut();
        let status = unsafe {
            ffi::VTDecompressionSessionCreate(
                ffi::kCFAllocatorDefault,
                format_description.as_ptr().cast(),
                ptr::null(),
                image_buffer_attributes.map_or(ptr::null(), |d| AsCFType::as_ptr(d).cast()),
                &record,
                &mut session,
            )
        };
        if status != 0 {
            // Recover the Arc so it doesn't leak.
            unsafe { Arc::from_raw(ref_con.cast::<CallbackState>()) };
            return Err(VTError::EncoderCallback(status));
        }
        Ok(Self { session, state })
    }

    /// Decode `sample_buffer`. The callback you registered at construction
    /// will fire with the resulting [`DecodedFrame`] — either synchronously
    /// before this method returns, or asynchronously after, depending on
    /// the decoder backend.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncoderCallback`] wrapping the raw `OSStatus`
    /// if `VTDecompressionSessionDecodeFrame` rejects the sample buffer.
    pub fn decode(&self, sample_buffer: &apple_cf::cm::CMSampleBuffer) -> Result<(), VTError> {
        self.decode_with_options(sample_buffer, 0, None).map(|_| ())
    }

    /// Decode `sample_buffer` with explicit `VTDecodeFrameFlags` and optional
    /// per-frame `kVTDecodeFrameOptionKey_*` entries.
    ///
    /// Returns the `VTDecodeInfoFlags` reported for the submit operation.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncoderCallback`] wrapping the raw `OSStatus`
    /// if VideoToolbox rejects the sample buffer, or `-4` (`unimpErr`) when
    /// `frame_options` are given on a system without
    /// `VTDecompressionSessionDecodeFrameWithOptions`.
    pub fn decode_with_options(
        &self,
        sample_buffer: &apple_cf::cm::CMSampleBuffer,
        decode_flags: ffi::VTDecodeFrameFlags,
        frame_options: Option<&CFDictionary>,
    ) -> Result<ffi::VTDecodeInfoFlags, VTError> {
        let mut info_flags: ffi::VTDecodeInfoFlags = 0;
        // Arcen patch: without per-frame options this is exactly
        // `VTDecompressionSessionDecodeFrame` (macOS 10.8+). The options
        // variant is resolved at runtime so the crate still loads on systems
        // that predate it; there it reports `UNIMPLEMENTED_STATUS`.
        let status = match frame_options {
            None => unsafe {
                ffi::VTDecompressionSessionDecodeFrame(
                    self.session,
                    sample_buffer.as_ptr().cast(),
                    decode_flags,
                    ptr::null_mut(),
                    &mut info_flags,
                )
            },
            Some(dict) => match decode_frame_with_options_fn() {
                Some(decode_frame_with_options) => unsafe {
                    decode_frame_with_options(
                        self.session,
                        sample_buffer.as_ptr().cast(),
                        decode_flags,
                        dict.as_ptr().cast_const().cast(),
                        ptr::null_mut(),
                        &mut info_flags,
                    )
                },
                None => UNIMPLEMENTED_STATUS,
            },
        };
        if status == 0 {
            Ok(info_flags)
        } else {
            Err(VTError::EncoderCallback(status))
        }
    }

    /// Submit `sample_buffer` for decoding and await the decoded `CVImageBuffer`.
    ///
    /// This method requires the crate's `async` feature.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncoderCallback`] if the decoder rejects the frame,
    /// reports an asynchronous failure, or completes without an image buffer.
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    #[allow(clippy::future_not_send)]
    pub async fn decode_frame_async(
        &self,
        sample_buffer: apple_cf::cm::CMSampleBuffer,
        frame_flags: u32,
    ) -> Result<apple_cf::cv::CVImageBuffer, VTError> {
        let (future, completion) = AsyncCompletion::<apple_cf::cv::CVImageBuffer>::create();
        let context = Box::into_raw(Box::new(AsyncDecodeContext(completion)));
        let status = unsafe {
            ffi::VTDecompressionSessionDecodeFrame(
                self.session,
                sample_buffer.as_ptr().cast(),
                frame_flags,
                context.cast::<c_void>(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            let context = unsafe { Box::from_raw(context) };
            unsafe {
                AsyncCompletion::<apple_cf::cv::CVImageBuffer>::complete_err(
                    context.0,
                    status.to_string(),
                );
            };
            return Err(VTError::EncoderCallback(status));
        }

        future
            .await
            .map_err(|error| VTError::EncoderCallback(parse_async_status(&error)))
    }

    /// Install a callback that receives multi-image output when the decoder
    /// produces a `CMTaggedBufferGroup` instead of a single `CVImageBuffer`.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the decoder rejects the callback.
    ///
    /// # Panics
    ///
    /// Panics if the callback mutex is poisoned.
    pub fn set_multi_image_callback<F>(&self, callback: F) -> Result<(), VTError>
    where
        F: FnMut(DecodedMultiImageFrame) + Send + 'static,
    {
        {
            let mut slot = self
                .state
                .multi_image_callback
                .lock()
                .expect("decoder multi-image callback mutex poisoned");
            *slot = Some(Box::new(callback));
        }

        let status = unsafe {
            ffi::VTDecompressionSessionSetMultiImageCallback(
                self.session,
                decode_multi_image_trampoline,
                Arc::as_ptr(&self.state).cast::<c_void>().cast_mut(),
            )
        };
        if status != 0 {
            let mut slot = self
                .state
                .multi_image_callback
                .lock()
                .expect("decoder multi-image callback mutex poisoned");
            *slot = None;
            drop(slot);
            return Err(VTError::ApiFailed {
                api: "VTDecompressionSessionSetMultiImageCallback",
                status,
            });
        }
        Ok(())
    }

    /// Wait for any queued async decodes to complete. Equivalent to
    /// `VTDecompressionSessionWaitForAsynchronousFrames`. Call this
    /// before dropping the session if you want to be sure every frame's
    /// callback has fired.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncoderCallback`] on a non-zero `OSStatus`.
    pub fn wait_for_async_frames(&self) -> Result<(), VTError> {
        let status = unsafe { ffi::VTDecompressionSessionWaitForAsynchronousFrames(self.session) };
        if status == 0 {
            Ok(())
        } else {
            Err(VTError::EncoderCallback(status))
        }
    }

    /// Copy a black pixel buffer matching the session's current output format.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] on a non-zero `OSStatus`.
    pub fn copy_black_pixel_buffer(&self) -> Result<apple_cf::cv::CVPixelBuffer, VTError> {
        let mut out = ptr::null_mut();
        let status =
            unsafe { ffi::VTDecompressionSessionCopyBlackPixelBuffer(self.session, &mut out) };
        if status != 0 || out.is_null() {
            return Err(VTError::ApiFailed {
                api: "VTDecompressionSessionCopyBlackPixelBuffer",
                status,
            });
        }
        apple_cf::cv::CVPixelBuffer::from_raw(out.cast()).ok_or(VTError::ApiFailed {
            api: "VTDecompressionSessionCopyBlackPixelBuffer",
            status,
        })
    }

    /// Copy one `VTSession` property from the decoder.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the underlying property query fails.
    ///
    /// # Safety
    ///
    /// `key` must be a valid CoreFoundation string pointer accepted by
    /// `VTSessionCopyProperty` for a decompression session.
    pub unsafe fn copy_property(&self, key: ffi::CFStringRef) -> Result<Option<CFType>, VTError> {
        session::copy_property(self.session.cast(), key)
    }

    /// Copy the decoder's supported-property dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    pub fn supported_property_dictionary(&self) -> Result<CFDictionary, VTError> {
        unsafe { session::copy_supported_property_dictionary(self.session.cast()) }
    }

    /// Copy the decoder's serializable property dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    pub fn serializable_properties(&self) -> Result<CFDictionary, VTError> {
        unsafe { session::copy_serializable_properties(self.session.cast()) }
    }

    /// Set multiple decoder properties at once.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if `VideoToolbox` rejects the dictionary.
    pub fn set_properties(&self, properties: &CFDictionary) -> Result<(), VTError> {
        unsafe { session::set_properties(self.session.cast(), properties) }
    }

    /// Set an arbitrary property on the underlying
    /// `VTDecompressionSession`. `value` must be a CoreFoundation
    /// object matching Apple's per-key contract (see
    /// `VTDecompressionProperties.h`).
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] if Apple rejects the
    /// key/value pair.
    ///
    /// # Safety
    ///
    /// `key` must be a valid `CFStringRef` and `value` must be a
    /// valid CoreFoundation pointer for the property's expected type.
    pub unsafe fn set_property(
        &self,
        key: ffi::CFStringRef,
        value: ffi::CFTypeRef,
    ) -> Result<(), VTError> {
        let status = ffi::VTSessionSetProperty(self.session.cast(), key, value);
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: "<custom>".to_string(),
                status,
            });
        }
        Ok(())
    }

    /// Tell the decoder that this is real-time playback (prioritise
    /// low latency over throughput). Wraps the boolean property
    /// `kVTDecompressionPropertyKey_RealTime`.
    ///
    /// # Errors
    ///
    /// See [`Self::set_property`].
    pub fn set_real_time(&self, real_time: bool) -> Result<(), VTError> {
        let v = unsafe {
            if real_time {
                ffi::kCFBooleanTrue
            } else {
                ffi::kCFBooleanFalse
            }
        };
        unsafe { self.set_property(ffi::kVTDecompressionPropertyKey_RealTime, v.cast()) }
    }

    /// Suggest a maximum number of frames the decoder may keep
    /// internally awaiting reordering (B-frame reorder depth).
    /// Wraps `kVTDecompressionPropertyKey_MaximumOutputBufferDepth`.
    ///
    /// # Errors
    ///
    /// See [`Self::set_property`].
    pub fn set_max_output_buffer_depth(&self, depth: i32) -> Result<(), VTError> {
        let v = unsafe {
            ffi::CFNumberCreate(
                ffi::kCFAllocatorDefault,
                ffi::kCFNumberSInt32Type,
                core::ptr::from_ref(&depth).cast(),
            )
        };
        let r = unsafe {
            self.set_property(
                ffi::kVTDecompressionPropertyKey_MaximumOutputBufferDepth,
                v.cast(),
            )
        };
        unsafe { ffi::CFRelease(v.cast()) };
        r
    }

    /// Finish any decoded frames the decoder has been holding back
    /// for B-frame reordering. Wraps
    /// `VTDecompressionSessionFinishDelayedFrames`.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncoderCallback`] on non-zero `OSStatus`.
    pub fn finish_delayed_frames(&self) -> Result<(), VTError> {
        let status = unsafe { ffi::VTDecompressionSessionFinishDelayedFrames(self.session) };
        if status == 0 {
            Ok(())
        } else {
            Err(VTError::EncoderCallback(status))
        }
    }

    /// True if this session can decode samples described by `format`
    /// without rebuilding the session. Wraps
    /// `VTDecompressionSessionCanAcceptFormatDescription`.
    ///
    /// # Safety
    ///
    /// `format` must be a valid `CMFormatDescriptionRef` (typically
    /// obtained from another `CMSampleBuffer`).
    #[must_use]
    pub unsafe fn can_accept_format(&self, format: ffi::CMFormatDescriptionRef) -> bool {
        ffi::VTDecompressionSessionCanAcceptFormatDescription(self.session, format)
    }
}

#[cfg(feature = "async")]
fn parse_async_status(error: &str) -> ffi::OSStatus {
    error.parse().unwrap_or(-1)
}

#[cfg(feature = "async")]
fn complete_async_decode(
    source_frame_ref_con: *mut c_void,
    status: ffi::OSStatus,
    image_buffer: *mut c_void,
) {
    catch_user_panic("videotoolbox::decompression::decode_frame_async", || {
        let context = unsafe { Box::from_raw(source_frame_ref_con.cast::<AsyncDecodeContext>()) };
        if status != 0 {
            unsafe {
                AsyncCompletion::<apple_cf::cv::CVImageBuffer>::complete_err(
                    context.0,
                    status.to_string(),
                );
            };
            return;
        }

        if image_buffer.is_null() {
            unsafe {
                AsyncCompletion::<apple_cf::cv::CVImageBuffer>::complete_err(
                    context.0,
                    "-1".into(),
                );
            };
            return;
        }

        unsafe { ffi::CFRetain(image_buffer.cast_const()) };
        let Some(image_buffer) = apple_cf::cv::CVImageBuffer::from_raw(image_buffer) else {
            unsafe {
                AsyncCompletion::<apple_cf::cv::CVImageBuffer>::complete_err(
                    context.0,
                    "-1".into(),
                );
            };
            return;
        };

        unsafe {
            AsyncCompletion::<apple_cf::cv::CVImageBuffer>::complete_ok(context.0, image_buffer);
        };
    });
}

unsafe extern "C" fn decode_trampoline(
    output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: ffi::OSStatus,
    info_flags: ffi::VTDecodeInfoFlags,
    image_buffer: *mut c_void,
    pts: ffi::CMTime,
    duration: ffi::CMTime,
) {
    #[cfg(not(feature = "async"))]
    let _ = source_frame_ref_con;
    #[cfg(feature = "async")]
    if !source_frame_ref_con.is_null() {
        complete_async_decode(source_frame_ref_con, status, image_buffer);
        return;
    }

    if output_ref_con.is_null() {
        return;
    }
    // Borrow the Arc without consuming the reference held by the session.
    let state = unsafe { Arc::from_raw(output_ref_con.cast::<CallbackState>()) };
    let state_clone = state.clone();
    core::mem::forget(state);

    let image = if image_buffer.is_null() {
        None
    } else {
        // The decoder owns the pixel buffer; retain so the Rust wrapper
        // can extend its lifetime past the callback's stack frame.
        unsafe { ffi::CFRetain(image_buffer.cast_const()) };
        apple_cf::cv::CVPixelBuffer::from_raw(image_buffer)
    };

    let frame = DecodedFrame {
        image_buffer: image,
        presentation_time: (pts.value, pts.timescale),
        duration: (duration.value, duration.timescale),
        info_flags,
        status,
    };

    let Ok(mut guard) = state_clone.callback.lock() else {
        return;
    };
    // The user closure may panic. Unwinding across this `extern "C"` boundary
    // back into VideoToolbox is undefined behaviour, so guard the invocation.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        guard(frame);
    }));
}

unsafe extern "C" fn decode_multi_image_trampoline(
    output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: ffi::OSStatus,
    info_flags: ffi::VTDecodeInfoFlags,
    tagged_buffer_group: ffi::CMTaggedBufferGroupRef,
    pts: ffi::CMTime,
    duration: ffi::CMTime,
) {
    if output_ref_con.is_null() {
        return;
    }
    let state = unsafe { Arc::from_raw(output_ref_con.cast::<CallbackState>()) };
    let state_clone = state.clone();
    core::mem::forget(state);

    let frame = DecodedMultiImageFrame {
        tagged_buffer_group: unsafe { TaggedBufferGroup::from_raw_retained(tagged_buffer_group) },
        presentation_time: (pts.value, pts.timescale),
        duration: (duration.value, duration.timescale),
        info_flags,
        status,
    };

    let Ok(mut guard) = state_clone.multi_image_callback.lock() else {
        return;
    };
    let Some(callback) = guard.as_mut() else {
        return;
    };
    // The user closure may panic; unwinding across this `extern "C"` boundary
    // back into VideoToolbox is undefined behaviour, so guard the invocation.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        callback(frame);
    }));
}
