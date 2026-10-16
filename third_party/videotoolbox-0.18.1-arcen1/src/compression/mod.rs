//! [`CompressionSession`] — hardware H.264/HEVC/ProRes encoder.

use core::ffi::c_void;
use core::ptr;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use apple_cf::cf::{CFDictionary, CFType};
use apple_cf::cv::CVPixelBufferPool;
use apple_cf::iosurface::IOSurface;
#[cfg(feature = "async")]
use doom_fish_utils::completion::{AsyncCompletion, SyncCompletionPtr};
#[cfg(feature = "async")]
use doom_fish_utils::panic_safe::catch_user_panic;

use crate::error::VTError;
use crate::ffi;
use crate::multipass::MultiPassStorage;
use crate::session::{self, Codec};
use crate::tagged_buffer_group::TaggedBufferGroup;

/// One encoded frame produced by [`CompressionSession::encode`].
///
/// Wraps the encoder's `CMSampleBuffer` as a safe [`apple_cf::cm::CMSampleBuffer`]
/// so downstream crates (e.g. `avassetwriter-rs`) can hand it off zero-copy
/// without dealing with raw `*mut c_void` pointers.
pub struct EncodedFrame {
    /// Encoded bitstream bytes (NAL units for H.264/HEVC, frame data for `ProRes`).
    pub data: Vec<u8>,
    /// Presentation timestamp of the source frame (numerator, timescale).
    pub presentation_time: (i64, i32),
    /// Encoder hint flags (e.g. dropped, asynchronous).
    pub info_flags: u32,
    /// Underlying `CoreMedia` sample buffer. `None` for dropped frames.
    sample_buffer: Option<apple_cf::cm::CMSampleBuffer>,
}

impl EncodedFrame {
    /// Borrow the encoded sample buffer for cross-crate hand-off (e.g. to
    /// `avassetwriter::Writer::append_sample`). Returns `None` for dropped
    /// frames.
    #[must_use]
    pub const fn cm_sample_buffer(&self) -> Option<&apple_cf::cm::CMSampleBuffer> {
        self.sample_buffer.as_ref()
    }

    /// Raw `CMSampleBufferRef` for code that bypasses `apple_cf` and talks
    /// directly to a Swift bridge via `extern "C"`.
    ///
    /// Returns `null` for dropped frames. Do **not** call `CFRelease` on
    /// the returned pointer — ownership stays with this `EncodedFrame`.
    #[must_use]
    pub fn cm_sample_buffer_ptr(&self) -> ffi::CMSampleBufferRef {
        self.sample_buffer
            .as_ref()
            .map_or(core::ptr::null_mut::<c_void>().cast(), |sample_buffer| {
                sample_buffer.as_ptr().cast()
            })
    }
}

impl Clone for EncodedFrame {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            presentation_time: self.presentation_time,
            info_flags: self.info_flags,
            sample_buffer: self.sample_buffer.clone(),
        }
    }
}

impl core::fmt::Debug for EncodedFrame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EncodedFrame")
            .field("len", &self.data.len())
            .field("presentation_time", &self.presentation_time)
            .field("info_flags", &self.info_flags)
            .field("sample_buffer", &self.sample_buffer)
            .finish()
    }
}

/// Builder for [`CompressionSession`].
///
/// # Example
///
/// ```rust,no_run
/// use videotoolbox::compression::CompressionSession;
/// use videotoolbox::session::Codec;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let session = CompressionSession::builder(1920, 1080, Codec::H264)
///     .with_real_time(true)
///     .with_average_bit_rate(8_000_000)
///     .with_expected_frame_rate(60.0)
///     .with_max_keyframe_interval(120)
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct CompressionSessionBuilder {
    width: i32,
    height: i32,
    codec: Codec,
    real_time: Option<bool>,
    allow_frame_reordering: Option<bool>,
    average_bit_rate: Option<i32>,
    expected_frame_rate: Option<f64>,
    max_keyframe_interval: Option<i32>,
    quality: Option<f32>,
    profile_level: Option<ProfileLevel>,
}

macro_rules! define_profile_levels {
    ($($variant:ident => $ffi_const:ident),+ $(,)?) => {
        /// Encoded profile/level for the underlying codec. Maps to
        /// `kVTProfileLevel_*` `CFStringRef` constants.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum ProfileLevel {
            $(
                $variant,
            )+
        }

        impl ProfileLevel {
            pub(crate) fn as_cf_string(self) -> ffi::CFStringRef {
                // SAFETY: FFI constants are statically defined by Apple's VideoToolbox SDK.
                // Returning them as immutable references is safe.
                unsafe {
                    match self {
                        $(
                            Self::$variant => ffi::$ffi_const,
                        )+
                    }
                }
            }
        }
    };
}

define_profile_levels! {
    H263Profile0Level10 => kVTProfileLevel_H263_Profile0_Level10,
    H263Profile0Level45 => kVTProfileLevel_H263_Profile0_Level45,
    H263Profile3Level45 => kVTProfileLevel_H263_Profile3_Level45,
    H264Baseline1_3 => kVTProfileLevel_H264_Baseline_1_3,
    H264Baseline3_0 => kVTProfileLevel_H264_Baseline_3_0,
    H264Baseline3_1 => kVTProfileLevel_H264_Baseline_3_1,
    H264Baseline3_2 => kVTProfileLevel_H264_Baseline_3_2,
    H264Baseline4_0 => kVTProfileLevel_H264_Baseline_4_0,
    H264Baseline4_1 => kVTProfileLevel_H264_Baseline_4_1,
    H264Baseline4_2 => kVTProfileLevel_H264_Baseline_4_2,
    H264Baseline5_0 => kVTProfileLevel_H264_Baseline_5_0,
    H264Baseline5_1 => kVTProfileLevel_H264_Baseline_5_1,
    H264Baseline5_2 => kVTProfileLevel_H264_Baseline_5_2,
    H264BaselineAutoLevel => kVTProfileLevel_H264_Baseline_AutoLevel,
    H264ConstrainedBaselineAutoLevel => kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
    H264ConstrainedHighAutoLevel => kVTProfileLevel_H264_ConstrainedHigh_AutoLevel,
    H264Extended5_0 => kVTProfileLevel_H264_Extended_5_0,
    H264ExtendedAutoLevel => kVTProfileLevel_H264_Extended_AutoLevel,
    H264High3_0 => kVTProfileLevel_H264_High_3_0,
    H264High3_1 => kVTProfileLevel_H264_High_3_1,
    H264High3_2 => kVTProfileLevel_H264_High_3_2,
    H264High4_0 => kVTProfileLevel_H264_High_4_0,
    H264High4_1 => kVTProfileLevel_H264_High_4_1,
    H264High4_2 => kVTProfileLevel_H264_High_4_2,
    H264High5_0 => kVTProfileLevel_H264_High_5_0,
    H264High5_1 => kVTProfileLevel_H264_High_5_1,
    H264High5_2 => kVTProfileLevel_H264_High_5_2,
    H264HighAutoLevel => kVTProfileLevel_H264_High_AutoLevel,
    H264Main3_0 => kVTProfileLevel_H264_Main_3_0,
    H264Main3_1 => kVTProfileLevel_H264_Main_3_1,
    H264Main3_2 => kVTProfileLevel_H264_Main_3_2,
    H264Main4_0 => kVTProfileLevel_H264_Main_4_0,
    H264Main4_1 => kVTProfileLevel_H264_Main_4_1,
    H264Main4_2 => kVTProfileLevel_H264_Main_4_2,
    H264Main5_0 => kVTProfileLevel_H264_Main_5_0,
    H264Main5_1 => kVTProfileLevel_H264_Main_5_1,
    H264Main5_2 => kVTProfileLevel_H264_Main_5_2,
    H264MainAutoLevel => kVTProfileLevel_H264_Main_AutoLevel,
    HEVCMainAutoLevel => kVTProfileLevel_HEVC_Main_AutoLevel,
    HEVCMain10AutoLevel => kVTProfileLevel_HEVC_Main10_AutoLevel,
    HEVCMain42210AutoLevel => kVTProfileLevel_HEVC_Main42210_AutoLevel,
    HEVCMonochromeAutoLevel => kVTProfileLevel_HEVC_Monochrome_AutoLevel,
    HEVCMonochrome10AutoLevel => kVTProfileLevel_HEVC_Monochrome10_AutoLevel,
    MP4VAdvancedSimpleL0 => kVTProfileLevel_MP4V_AdvancedSimple_L0,
    MP4VAdvancedSimpleL1 => kVTProfileLevel_MP4V_AdvancedSimple_L1,
    MP4VAdvancedSimpleL2 => kVTProfileLevel_MP4V_AdvancedSimple_L2,
    MP4VAdvancedSimpleL3 => kVTProfileLevel_MP4V_AdvancedSimple_L3,
    MP4VAdvancedSimpleL4 => kVTProfileLevel_MP4V_AdvancedSimple_L4,
    MP4VMainL2 => kVTProfileLevel_MP4V_Main_L2,
    MP4VMainL3 => kVTProfileLevel_MP4V_Main_L3,
    MP4VMainL4 => kVTProfileLevel_MP4V_Main_L4,
    MP4VSimpleL0 => kVTProfileLevel_MP4V_Simple_L0,
    MP4VSimpleL1 => kVTProfileLevel_MP4V_Simple_L1,
    MP4VSimpleL2 => kVTProfileLevel_MP4V_Simple_L2,
    MP4VSimpleL3 => kVTProfileLevel_MP4V_Simple_L3,
}

impl CompressionSessionBuilder {
    #[must_use]
    pub const fn new(width: i32, height: i32, codec: Codec) -> Self {
        Self {
            width,
            height,
            codec,
            real_time: None,
            allow_frame_reordering: None,
            average_bit_rate: None,
            expected_frame_rate: None,
            max_keyframe_interval: None,
            quality: None,
            profile_level: None,
        }
    }

    /// Hint to the encoder that frames are arriving in real-time (e.g. live
    /// capture) and it should prioritise low latency over compression efficiency.
    #[must_use]
    pub const fn with_real_time(mut self, real_time: bool) -> Self {
        self.real_time = Some(real_time);
        self
    }

    /// Allow B-frame reordering (better compression, higher latency).
    #[must_use]
    pub const fn with_allow_frame_reordering(mut self, allow: bool) -> Self {
        self.allow_frame_reordering = Some(allow);
        self
    }

    /// Target average bitrate in bits-per-second.
    #[must_use]
    pub const fn with_average_bit_rate(mut self, bps: i32) -> Self {
        self.average_bit_rate = Some(bps);
        self
    }

    /// Expected source frame rate. Helps the encoder size its rate-control window.
    #[must_use]
    pub const fn with_expected_frame_rate(mut self, fps: f64) -> Self {
        self.expected_frame_rate = Some(fps);
        self
    }

    /// Force a keyframe at most every `n` frames.
    #[must_use]
    pub const fn with_max_keyframe_interval(mut self, n: i32) -> Self {
        self.max_keyframe_interval = Some(n);
        self
    }

    /// Encoding quality hint, `0.0..=1.0`. `0.0` minimum quality / size,
    /// `1.0` maximum. Wraps `kVTCompressionPropertyKey_Quality`.
    #[must_use]
    pub const fn with_quality(mut self, quality: f32) -> Self {
        self.quality = Some(quality);
        self
    }

    /// Profile/level for the encoded stream. See [`ProfileLevel`].
    /// Wraps `kVTCompressionPropertyKey_ProfileLevel`.
    #[must_use]
    pub const fn with_profile_level(mut self, profile: ProfileLevel) -> Self {
        self.profile_level = Some(profile);
        self
    }

    /// Construct the session and apply all configured properties.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::InvalidArgument`] if width/height are non-positive,
    /// [`VTError::SessionCreateFailed`] if `VideoToolbox` refuses to instantiate,
    /// or [`VTError::SetPropertyFailed`] if any property is rejected.
    pub fn build(self) -> Result<CompressionSession, VTError> {
        if self.width <= 0 || self.height <= 0 {
            return Err(VTError::InvalidArgument(format!(
                "width/height must be positive (got {}x{})",
                self.width, self.height
            )));
        }
        CompressionSession::new_internal(&self)
    }
}

/// Hardware-accelerated video compression session.
///
/// Construct via [`CompressionSession::builder`]. Each session owns a Swift-side
/// `VTCompressionSessionRef`; the underlying encoder is invalidated on drop.
pub struct CompressionSession {
    session: ffi::VTCompressionSessionRef,
    state: Arc<EncoderState>,
}

// SAFETY: VideoToolbox sessions are documented as thread-safe for concurrent
// `encode` calls; the underlying encoder owns its own dispatch queue.
unsafe impl Send for CompressionSession {}
unsafe impl Sync for CompressionSession {}

struct EncoderState {
    out_tx: Mutex<mpsc::Sender<Result<EncodedFrame, VTError>>>,
    out_rx: Mutex<mpsc::Receiver<Result<EncodedFrame, VTError>>>,
}

#[cfg(feature = "async")]
struct AsyncEncodeContext(SyncCompletionPtr);

impl CompressionSession {
    /// Convenience: start a builder.
    #[must_use]
    pub const fn builder(width: i32, height: i32, codec: Codec) -> CompressionSessionBuilder {
        CompressionSessionBuilder::new(width, height, codec)
    }

    /// CoreFoundation type identifier for `VTCompressionSession`.
    #[must_use]
    pub fn type_id() -> usize {
        // SAFETY: `VTCompressionSessionGetTypeID` is a standard Apple SDK function
        // that returns a static type ID. Safe to call from any thread.
        unsafe { ffi::VTCompressionSessionGetTypeID() }
    }

    /// Returns `true` when the current system advertises stereo MV-HEVC
    /// encode support.
    #[must_use]
    pub fn is_stereo_mvhevc_encode_supported() -> bool {
        // SAFETY: `VTIsStereoMVHEVCEncodeSupported` is a standard query function
        // that performs no I/O and has no side effects.
        unsafe { ffi::VTIsStereoMVHEVCEncodeSupported() != 0 }
    }

    /// Returns the current source-pixel-buffer pool, retaining it so the
    /// returned wrapper owns its lifetime independently of the session.
    #[must_use]
    pub fn pixel_buffer_pool(&self) -> Option<CVPixelBufferPool> {
        // SAFETY: `VTCompressionSessionGetPixelBufferPool` returns a borrowed reference
        // to the pool (not retained). We call CFRetain to extend its lifetime.
        let pool = unsafe { ffi::VTCompressionSessionGetPixelBufferPool(self.session) };
        if pool.is_null() {
            return None;
        }
        // SAFETY: Incrementing the retain count on a valid CFType is safe.
        unsafe { ffi::CFRetain(pool.cast()) };
        CVPixelBufferPool::from_raw(pool.cast())
    }

    /// Copy one `VTSession` property from the encoder.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the underlying property query fails.
    ///
    /// # Safety
    ///
    /// `key` must be a valid CoreFoundation string pointer accepted by
    /// `VTSessionCopyProperty` for a compression session.
    pub unsafe fn copy_property(&self, key: ffi::CFStringRef) -> Result<Option<CFType>, VTError> {
        session::copy_property(self.session.cast(), key)
    }

    /// Copy the encoder's supported-property dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    pub fn supported_property_dictionary(&self) -> Result<CFDictionary, VTError> {
        // SAFETY: `copy_supported_property_dictionary` is called with a valid session pointer.
        unsafe { session::copy_supported_property_dictionary(self.session.cast()) }
    }

    /// Copy the encoder's serializable property dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    pub fn serializable_properties(&self) -> Result<CFDictionary, VTError> {
        // SAFETY: `copy_serializable_properties` is called with a valid session pointer.
        unsafe { session::copy_serializable_properties(self.session.cast()) }
    }

    /// Set multiple encoder properties at once.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if `VideoToolbox` rejects the dictionary.
    pub fn set_properties(&self, properties: &CFDictionary) -> Result<(), VTError> {
        // SAFETY: `set_properties` is called with a valid session pointer and a valid dictionary.
        unsafe { session::set_properties(self.session.cast(), properties) }
    }

    /// Attach a [`MultiPassStorage`] object so the session can do multi-pass
    /// encoding.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] if `VideoToolbox` rejects the
    /// storage object.
    pub fn set_multi_pass_storage(&self, storage: &MultiPassStorage) -> Result<(), VTError> {
        unsafe {
            self.set_property(
                ffi::kVTCompressionPropertyKey_MultiPassStorage,
                storage.as_ptr().cast(),
            )
        }
    }

    /// Begin a multi-pass encoding pass.
    ///
    /// Set `final_pass` when you know this must be the last pass.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] on a non-zero `OSStatus`.
    pub fn begin_pass(&self, final_pass: bool) -> Result<(), VTError> {
        let flags = if final_pass {
            ffi::kVTCompressionSessionBeginFinalPass
        } else {
            0
        };
        let status =
            unsafe { ffi::VTCompressionSessionBeginPass(self.session, flags, ptr::null_mut()) };
        if status == 0 {
            Ok(())
        } else {
            Err(VTError::ApiFailed {
                api: "VTCompressionSessionBeginPass",
                status,
            })
        }
    }

    /// End the current multi-pass encoding pass.
    ///
    /// Returns `true` when the encoder requests another pass.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] on a non-zero `OSStatus`.
    pub fn end_pass(&self) -> Result<bool, VTError> {
        let mut further_passes_requested: ffi::Boolean = 0;
        let status = unsafe {
            ffi::VTCompressionSessionEndPass(
                self.session,
                (&raw mut further_passes_requested).cast(),
                ptr::null_mut(),
            )
        };
        if status == 0 {
            Ok(further_passes_requested != 0)
        } else {
            Err(VTError::ApiFailed {
                api: "VTCompressionSessionEndPass",
                status,
            })
        }
    }

    /// Return the time ranges the encoder wants on the next multi-pass pass.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] on a non-zero `OSStatus`.
    pub fn time_ranges_for_next_pass(&self) -> Result<Vec<ffi::CMTimeRange>, VTError> {
        let mut count: ffi::CMItemCount = 0;
        let mut ranges: *const ffi::CMTimeRange = ptr::null();
        let status = unsafe {
            ffi::VTCompressionSessionGetTimeRangesForNextPass(self.session, &mut count, &mut ranges)
        };
        if status != 0 {
            return Err(VTError::ApiFailed {
                api: "VTCompressionSessionGetTimeRangesForNextPass",
                status,
            });
        }
        if count <= 0 || ranges.is_null() {
            return Ok(Vec::new());
        }
        let count = usize::try_from(count).map_err(|_| {
            VTError::InvalidArgument("time-range count overflowed usize".to_string())
        })?;
        Ok(unsafe { std::slice::from_raw_parts(ranges, count) }.to_vec())
    }

    fn new_internal(b: &CompressionSessionBuilder) -> Result<Self, VTError> {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(EncoderState {
            out_tx: Mutex::new(tx),
            out_rx: Mutex::new(rx),
        });
        let state_for_callback = state.clone();
        let callback_ref_con = Arc::into_raw(state_for_callback)
            .cast::<c_void>()
            .cast_mut();

        let mut session_ptr: ffi::VTCompressionSessionRef = ptr::null_mut();
        // SAFETY: `VTCompressionSessionCreate` is a standard Apple SDK function.
        // All arguments are valid pointers/values: allocator is default, callback is a valid
        // C function, ref_con points to an Arc that will be owned by the callback,
        // and session_ptr is uninitialized but properly initialized on return.
        let status = unsafe {
            ffi::VTCompressionSessionCreate(
                ffi::kCFAllocatorDefault,
                b.width,
                b.height,
                b.codec.as_cm_codec_type(),
                ptr::null(),
                ptr::null(),
                ffi::kCFAllocatorDefault,
                Some(encode_callback),
                callback_ref_con,
                &mut session_ptr,
            )
        };
        if status != 0 || session_ptr.is_null() {
            // Drop the leaked Arc clone since the encoder will never call us back.
            // SAFETY: This Arc was created via `Arc::into_raw` below and leaked to VideoToolbox.
            // If initialization fails, we must recover and drop it to avoid a leak.
            unsafe { drop(Arc::from_raw(callback_ref_con.cast::<EncoderState>())) };
            return Err(VTError::SessionCreateFailed(status));
        }

        let session = Self {
            session: session_ptr,
            state,
        };

        // Apply properties.
        if let Some(rt) = b.real_time {
            session.set_property_bool(
                unsafe { ffi::kVTCompressionPropertyKey_RealTime },
                "RealTime",
                rt,
            )?;
        }
        if let Some(allow) = b.allow_frame_reordering {
            session.set_property_bool(
                unsafe { ffi::kVTCompressionPropertyKey_AllowFrameReordering },
                "AllowFrameReordering",
                allow,
            )?;
        }
        if let Some(bps) = b.average_bit_rate {
            session.set_property_i32(
                unsafe { ffi::kVTCompressionPropertyKey_AverageBitRate },
                "AverageBitRate",
                bps,
            )?;
        }
        if let Some(fps) = b.expected_frame_rate {
            session.set_property_f64(
                unsafe { ffi::kVTCompressionPropertyKey_ExpectedFrameRate },
                "ExpectedFrameRate",
                fps,
            )?;
        }
        if let Some(n) = b.max_keyframe_interval {
            session.set_property_i32(
                unsafe { ffi::kVTCompressionPropertyKey_MaxKeyFrameInterval },
                "MaxKeyFrameInterval",
                n,
            )?;
        }
        if let Some(q) = b.quality {
            session.set_property_f64(
                unsafe { ffi::kVTCompressionPropertyKey_Quality },
                "Quality",
                f64::from(q.clamp(0.0, 1.0)),
            )?;
        }
        if let Some(profile) = b.profile_level {
            session.set_property_cf_string(
                unsafe { ffi::kVTCompressionPropertyKey_ProfileLevel },
                "ProfileLevel",
                profile.as_cf_string(),
            )?;
        }

        let status = unsafe { ffi::VTCompressionSessionPrepareToEncodeFrames(session.session) };
        if status != 0 {
            return Err(VTError::PrepareFailed(status));
        }

        Ok(session)
    }

    /// Submit one frame for encoding and block until the encoder has emitted it.
    ///
    /// `presentation_time` is `(value, timescale)`, e.g. `(0, 30)` for the first
    /// frame of a 30 fps stream and `(1, 30)` for the second.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::PixelBufferCreateFailed`] if the `IOSurface` can't be
    /// wrapped, [`VTError::EncodeFailed`] if the encoder rejects the frame, or
    /// [`VTError::EncoderCallback`] if the encoder reports a non-zero status
    /// asynchronously.
    ///
    /// # Panics
    ///
    /// Panics if the encoder's internal callback receiver mutex is poisoned
    /// (only possible if a previous callback panicked while holding it).
    pub fn encode(
        &self,
        surface: &IOSurface,
        presentation_time: (i64, i32),
    ) -> Result<EncodedFrame, VTError> {
        let pixel_buffer = self.wrap_iosurface(surface)?;
        let pts = ffi::CMTime::new(presentation_time.0, presentation_time.1);
        let status = unsafe {
            ffi::VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer,
                pts,
                ffi::CMTime::INVALID,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        unsafe { ffi::CFRelease(pixel_buffer.cast()) };
        self.finish_encode(status)
    }

    /// Submit `image_buffer` for encoding and await the encoded `CMSampleBuffer`.
    ///
    /// This method requires the crate's `async` feature.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] if the frame submission is rejected or
    /// [`VTError::EncoderCallback`] if the encoder callback reports a failure or
    /// drops the frame without a `CMSampleBuffer`.
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    #[allow(clippy::future_not_send)]
    pub async fn encode_frame_async(
        &self,
        image_buffer: apple_cf::cv::CVPixelBuffer,
        presentation_timestamp: apple_cf::cm::CMTime,
        duration: apple_cf::cm::CMTime,
        frame_properties: Option<CFDictionary>,
    ) -> Result<apple_cf::cm::CMSampleBuffer, VTError> {
        let (future, completion) = AsyncCompletion::<apple_cf::cm::CMSampleBuffer>::create();
        let context = Box::into_raw(Box::new(AsyncEncodeContext(completion)));
        let frame_properties_ref = frame_properties.as_ref();
        let status = unsafe {
            ffi::VTCompressionSessionEncodeFrame(
                self.session,
                image_buffer.as_ptr().cast(),
                presentation_timestamp,
                duration,
                frame_properties_ref.map_or(ptr::null(), |dict| dict.as_ptr().cast_const().cast()),
                context.cast::<c_void>(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            let context = unsafe { Box::from_raw(context) };
            unsafe {
                AsyncCompletion::<apple_cf::cm::CMSampleBuffer>::complete_err(
                    context.0,
                    status.to_string(),
                );
            };
            return Err(VTError::EncodeFailed(status));
        }

        future
            .await
            .map_err(|error| VTError::EncoderCallback(parse_async_status(&error)))
    }

    /// Submit one multi-image frame (for example stereo MV-HEVC left/right eye
    /// images) and block until the encoder has emitted it.
    ///
    /// The `tagged_buffer_group` is a `CoreMedia` `CMTaggedBufferGroup` containing
    /// the images that make up one logical frame.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] if the encoder rejects the frame or
    /// [`VTError::EncoderCallback`] if the completion callback reports failure.
    pub fn encode_multi_image(
        &self,
        tagged_buffer_group: &TaggedBufferGroup,
        presentation_time: (i64, i32),
    ) -> Result<EncodedFrame, VTError> {
        let pts = ffi::CMTime::new(presentation_time.0, presentation_time.1);
        let status = unsafe {
            ffi::VTCompressionSessionEncodeMultiImageFrame(
                self.session,
                tagged_buffer_group.as_ptr(),
                pts,
                ffi::CMTime::INVALID,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        self.finish_encode(status)
    }

    fn finish_encode(&self, encode_status: ffi::OSStatus) -> Result<EncodedFrame, VTError> {
        if encode_status != 0 {
            return Err(VTError::EncodeFailed(encode_status));
        }

        let complete_status =
            unsafe { ffi::VTCompressionSessionCompleteFrames(self.session, ffi::CMTime::INVALID) };
        if complete_status != 0 {
            return Err(VTError::CompleteFailed(complete_status));
        }

        let rx = self.state.out_rx.lock().expect("encoder rx mutex poisoned");
        rx.recv().map_err(|_| VTError::EncoderCallback(-1))?
    }

    #[allow(clippy::unused_self)]
    fn wrap_iosurface(&self, surface: &IOSurface) -> Result<ffi::CVPixelBufferRef, VTError> {
        let mut pb: ffi::CVPixelBufferRef = ptr::null_mut();
        let status = unsafe {
            ffi::CVPixelBufferCreateWithIOSurface(
                ffi::kCFAllocatorDefault,
                surface.as_ptr().cast::<c_void>(),
                ptr::null(),
                &mut pb,
            )
        };
        if status != 0 || pb.is_null() {
            return Err(VTError::PixelBufferCreateFailed(status));
        }
        Ok(pb)
    }

    fn set_property_bool(
        &self,
        key: ffi::CFStringRef,
        key_name: &'static str,
        value: bool,
    ) -> Result<(), VTError> {
        let cf_value = if value {
            unsafe { ffi::kCFBooleanTrue }
        } else {
            unsafe { ffi::kCFBooleanFalse }
        };
        let status = unsafe { ffi::VTSessionSetProperty(self.session, key, cf_value.cast()) };
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: key_name.to_string(),
                status,
            });
        }
        Ok(())
    }

    fn set_property_i32(
        &self,
        key: ffi::CFStringRef,
        key_name: &'static str,
        value: i32,
    ) -> Result<(), VTError> {
        let value_ref = unsafe {
            ffi::CFNumberCreate(
                ffi::kCFAllocatorDefault,
                ffi::kCFNumberSInt32Type,
                core::ptr::from_ref(&value).cast(),
            )
        };
        let status = unsafe { ffi::VTSessionSetProperty(self.session, key, value_ref.cast()) };
        unsafe { ffi::CFRelease(value_ref.cast()) };
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: key_name.to_string(),
                status,
            });
        }
        Ok(())
    }

    fn set_property_f64(
        &self,
        key: ffi::CFStringRef,
        key_name: &'static str,
        value: f64,
    ) -> Result<(), VTError> {
        let value_ref = unsafe {
            ffi::CFNumberCreate(
                ffi::kCFAllocatorDefault,
                ffi::kCFNumberFloat64Type,
                core::ptr::from_ref(&value).cast(),
            )
        };
        let status = unsafe { ffi::VTSessionSetProperty(self.session, key, value_ref.cast()) };
        unsafe { ffi::CFRelease(value_ref.cast()) };
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: key_name.to_string(),
                status,
            });
        }
        Ok(())
    }

    fn set_property_cf_string(
        &self,
        key: ffi::CFStringRef,
        key_name: &'static str,
        value: ffi::CFStringRef,
    ) -> Result<(), VTError> {
        let status = unsafe { ffi::VTSessionSetProperty(self.session, key, value.cast()) };
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: key_name.to_string(),
                status,
            });
        }
        Ok(())
    }

    /// Set an arbitrary property on the underlying `VTCompressionSession`.
    /// `value` must be a CoreFoundation object (`CFNumberRef`,
    /// `CFBooleanRef`, `CFStringRef`, ...).
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] if Apple rejects the
    /// key/value pair.
    ///
    /// # Safety
    ///
    /// `key` must be a valid `CFStringRef`; `value` must be a valid
    /// CoreFoundation object pointer for the property's expected type
    /// (per Apple's `VTCompressionProperties.h`).
    pub unsafe fn set_property(
        &self,
        key: ffi::CFStringRef,
        value: ffi::CFTypeRef,
    ) -> Result<(), VTError> {
        let status = ffi::VTSessionSetProperty(self.session, key, value);
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: "<custom>".to_string(),
                status,
            });
        }
        Ok(())
    }
}

impl Drop for CompressionSession {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe {
                ffi::VTCompressionSessionInvalidate(self.session);
                ffi::CFRelease(self.session.cast());
            }
        }
        // The Arc<EncoderState> we leaked into VTCompressionSessionCreate's
        // ref-con cannot be retrieved here cleanly without unsafe gymnastics —
        // VideoToolbox doesn't surface the pointer back. Leak it; it's tiny and
        // sessions are long-lived.
        //
        // TODO: bind to VTCompressionSessionGetRefCon if/when we can rely on it
        // being available.
    }
}

impl core::fmt::Debug for CompressionSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CompressionSession")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

// ---- internal callback ----

#[cfg(feature = "async")]
fn parse_async_status(error: &str) -> ffi::OSStatus {
    error.parse().unwrap_or(-1)
}

#[cfg(feature = "async")]
fn complete_async_encode(
    source_frame_ref_con: *mut c_void,
    status: ffi::OSStatus,
    sample_buffer: ffi::CMSampleBufferRef,
) {
    catch_user_panic("videotoolbox::compression::encode_frame_async", || {
        let context = unsafe { Box::from_raw(source_frame_ref_con.cast::<AsyncEncodeContext>()) };
        if status != 0 {
            unsafe {
                AsyncCompletion::<apple_cf::cm::CMSampleBuffer>::complete_err(
                    context.0,
                    status.to_string(),
                );
            };
            return;
        }

        let Some(sample_buffer) =
            (unsafe { apple_cf::cm::CMSampleBuffer::from_raw_retained(sample_buffer.cast()) })
        else {
            unsafe {
                AsyncCompletion::<apple_cf::cm::CMSampleBuffer>::complete_err(
                    context.0,
                    "-1".into(),
                );
            };
            return;
        };

        unsafe {
            AsyncCompletion::<apple_cf::cm::CMSampleBuffer>::complete_ok(context.0, sample_buffer);
        };
    });
}

unsafe extern "C" fn encode_callback(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: ffi::OSStatus,
    info_flags: ffi::VTEncodeInfoFlags,
    sample_buffer: ffi::CMSampleBufferRef,
) {
    #[cfg(not(feature = "async"))]
    let _ = source_frame_ref_con;
    #[cfg(feature = "async")]
    if !source_frame_ref_con.is_null() {
        complete_async_encode(source_frame_ref_con, status, sample_buffer);
        return;
    }

    // We borrow but do not consume the Arc — the session keeps it alive for
    // the lifetime of the encoder.
    let state_ptr = output_callback_ref_con.cast::<EncoderState>();
    let state = Arc::from_raw(state_ptr);
    let state_clone = state.clone();
    core::mem::forget(state); // restore refcount

    let result = if status != 0 {
        Err(VTError::EncoderCallback(status))
    } else if sample_buffer.is_null() {
        // Frame was dropped — emit an empty frame with the dropped flag set.
        Ok(EncodedFrame {
            data: Vec::new(),
            presentation_time: (0, 0),
            info_flags,
            sample_buffer: None,
        })
    } else {
        let pts = ffi::CMSampleBufferGetPresentationTimeStamp(sample_buffer);
        let block_buffer = ffi::CMSampleBufferGetDataBuffer(sample_buffer);
        if block_buffer.is_null() {
            Err(VTError::EncoderCallback(-2))
        } else {
            let len = ffi::CMBlockBufferGetDataLength(block_buffer);
            let mut data = vec![0u8; len];
            let copy_status = ffi::CMBlockBufferCopyDataBytes(
                block_buffer,
                0,
                len,
                data.as_mut_ptr().cast::<c_void>(),
            );
            if copy_status != 0 {
                Err(VTError::EncoderCallback(copy_status))
            } else {
                // Wrap the CMSampleBuffer in a safe apple_cf type. The
                // wrapper retains-on-take so the encoder's reference is
                // unaffected.
                let safe = apple_cf::cm::CMSampleBuffer::from_raw_retained(sample_buffer.cast());
                Ok(EncodedFrame {
                    data,
                    presentation_time: (pts.value, pts.timescale),
                    info_flags,
                    sample_buffer: safe,
                })
            }
        }
    };

    // Avoid panicking across the FFI boundary if the mutex is poisoned.
    if let Ok(tx) = state_clone.out_tx.lock() {
        let _ = tx.send(result);
    };
}

#[cfg(test)]
mod tests {
    use super::{CompressionSessionBuilder, EncodedFrame, ProfileLevel};
    use crate::{error::VTError, ffi, session::Codec};

    #[test]
    fn builder_new_starts_with_expected_defaults() {
        let builder = CompressionSessionBuilder::new(1920, 1080, Codec::H264);

        assert_eq!(builder.width, 1920);
        assert_eq!(builder.height, 1080);
        assert_eq!(builder.codec, Codec::H264);
        assert_eq!(builder.real_time, None);
        assert_eq!(builder.allow_frame_reordering, None);
        assert_eq!(builder.average_bit_rate, None);
        assert_eq!(builder.expected_frame_rate, None);
        assert_eq!(builder.max_keyframe_interval, None);
        assert_eq!(builder.quality, None);
        assert_eq!(builder.profile_level, None);
    }

    #[test]
    fn builder_chain_records_requested_settings() {
        let builder = CompressionSessionBuilder::new(3840, 2160, Codec::HEVC)
            .with_real_time(true)
            .with_allow_frame_reordering(false)
            .with_average_bit_rate(24_000_000)
            .with_expected_frame_rate(59.94)
            .with_max_keyframe_interval(120)
            .with_quality(0.75)
            .with_profile_level(ProfileLevel::HEVCMain10AutoLevel);

        assert_eq!(builder.width, 3840);
        assert_eq!(builder.height, 2160);
        assert_eq!(builder.codec, Codec::HEVC);
        assert_eq!(builder.real_time, Some(true));
        assert_eq!(builder.allow_frame_reordering, Some(false));
        assert_eq!(builder.average_bit_rate, Some(24_000_000));
        assert!(
            (builder
                .expected_frame_rate
                .expect("expected frame rate should be set")
                - 59.94)
                .abs()
                < 1.0e-9
        );
        assert_eq!(builder.max_keyframe_interval, Some(120));
        assert!(
            (builder.quality.expect("quality should be set") - 0.75).abs() < f32::EPSILON
        );
        assert_eq!(builder.profile_level, Some(ProfileLevel::HEVCMain10AutoLevel));
    }

    #[test]
    fn build_rejects_non_positive_dimensions_before_entering_ffi() {
        let error = CompressionSessionBuilder::new(0, 1080, Codec::H264)
            .build()
            .expect_err("zero width must fail before session creation");

        assert_eq!(
            error,
            VTError::InvalidArgument("width/height must be positive (got 0x1080)".to_owned())
        );
    }

    #[test]
    fn encoded_frame_accessors_handle_missing_sample_buffer() {
        let frame = EncodedFrame {
            data: vec![1, 2, 3],
            presentation_time: (10, 30),
            info_flags: 7,
            sample_buffer: None,
        };
        let cloned = frame.clone();
        let debug = format!("{cloned:?}");

        assert!(frame.cm_sample_buffer().is_none());
        assert!(frame.cm_sample_buffer_ptr().is_null());
        assert_eq!(cloned.data, vec![1, 2, 3]);
        assert_eq!(cloned.presentation_time, (10, 30));
        assert_eq!(cloned.info_flags, 7);
        assert!(debug.contains("EncodedFrame"));
        assert!(debug.contains("presentation_time"));
    }

    #[test]
    fn profile_level_maps_to_expected_cfstring_constants() {
        assert_eq!(
            ProfileLevel::H264HighAutoLevel.as_cf_string(),
            unsafe { ffi::kVTProfileLevel_H264_High_AutoLevel }
        );
        assert_eq!(
            ProfileLevel::HEVCMain10AutoLevel.as_cf_string(),
            unsafe { ffi::kVTProfileLevel_HEVC_Main10_AutoLevel }
        );
    }

    #[test]
    fn compression_property_keys_are_non_null_and_distinct() {
        let real_time = unsafe { ffi::kVTCompressionPropertyKey_RealTime };
        let profile_level = unsafe { ffi::kVTCompressionPropertyKey_ProfileLevel };
        let quality = unsafe { ffi::kVTCompressionPropertyKey_Quality };

        assert!(!real_time.is_null());
        assert!(!profile_level.is_null());
        assert!(!quality.is_null());
        assert_ne!(real_time, profile_level);
        assert_ne!(real_time, quality);
        assert_ne!(profile_level, quality);
    }
}
