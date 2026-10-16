//! Async API for `videotoolbox`.
//!
//! Provides executor-agnostic wrappers around the crate's callback-shaped APIs.
//! Enable with the `async` cargo feature.
//!
//! ## Available types
//!
//! | Type | Wrapped surface |
//! |------|-----------------|
//! | [`AsyncCompressionSession`] | single-frame `VTCompressionSession` completion |
//! | [`AsyncDecompressionSession`] | single-frame `VTDecompressionSession` completion |
//! | [`AsyncRawProcessingSession`] | `VTRAWProcessingSession` async processing plus parameter-change stream subscription |
//! | [`RawProcessingParameterChangeStream`] | `VTRAWProcessingSessionSetParameterChangedHandler` |
//!
//! ## Tier-2 deferrals
//!
//! The following callback-shaped APIs are intentionally **not** exposed as
//! async streams here:
//!
//! * [`crate::DecompressionSession::set_multi_image_callback`] — the audited C
//!   signature requires a **non-null** callback and does not provide a clear /
//!   unsubscribe entry point, so an RAII stream wrapper would have to install a
//!   hidden no-op callback on drop.
//! * [`crate::FrameSilo::sample_buffers`] — synchronous collection helper.
//! * [`crate::MultiPassStorage::close`] and time-range configuration APIs —
//!   synchronous control surfaces.
//!
//! ## Example
//!
//! ```rust,no_run
//! use apple_cf::cm::CMTime;
//! use apple_cf::cv::CVPixelBuffer;
//! use videotoolbox::{Codec, CompressionSession, async_api::AsyncCompressionSession};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! pollster::block_on(async {
//!     let session = CompressionSession::builder(64, 64, Codec::H264)
//!         .with_real_time(true)
//!         .with_average_bit_rate(500_000)
//!         .with_expected_frame_rate(30.0)
//!         .with_max_keyframe_interval(1)
//!         .build()?;
//!     let frame = CVPixelBuffer::create(64, 64, u32::from_be_bytes(*b"BGRA"))
//!         .unwrap_or_else(|status| panic!("CVPixelBuffer::create failed: {status}"));
//!     let sample_buffer = AsyncCompressionSession::new(&session)
//!         .encode_frame(frame, CMTime::new(0, 30), CMTime::INVALID, None)
//!         .await?;
//!     assert!(sample_buffer.is_valid());
//!     Ok::<_, videotoolbox::VTError>(())
//! })?;
//! # Ok(())
//! # }
//! ```

#![cfg(feature = "async")]

use core::future::Future;

use apple_cf::cm::CMSampleBuffer;
use apple_cf::cv::CVImageBuffer;

#[cfg(any(feature = "compression", feature = "frame_processor"))]
use apple_cf::cv::CVPixelBuffer;
#[cfg(feature = "compression")]
use apple_cf::{cf::CFDictionary, cm::CMTime};
#[cfg(feature = "frame_processor")]
use doom_fish_utils::{
    panic_safe::catch_user_panic,
    stream::{BoundedAsyncStream, NextItem},
};

#[cfg(feature = "compression")]
use crate::compression::CompressionSession;
#[cfg(feature = "frame_processor")]
use crate::raw_processing::{RawProcessingParameter, RawProcessingSession};
use crate::{DecompressionSession, VTError};

/// Async accessor for [`CompressionSession`].
///
/// Wraps the encoder's one-shot frame completion callback as a future that can
/// be awaited on any executor.
#[cfg(feature = "compression")]
#[cfg_attr(docsrs, doc(cfg(all(feature = "async", feature = "compression"))))]
pub struct AsyncCompressionSession<'a> {
    session: &'a CompressionSession,
}

#[cfg(feature = "compression")]
impl core::fmt::Debug for AsyncCompressionSession<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncCompressionSession")
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "compression")]
impl<'a> AsyncCompressionSession<'a> {
    /// Wrap a borrowed [`CompressionSession`].
    #[must_use]
    pub const fn new(session: &'a CompressionSession) -> Self {
        Self { session }
    }

    /// Submit `image_buffer` for encoding and await the encoded sample buffer.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] if the frame submission is rejected or
    /// [`VTError::EncoderCallback`] if the encoder callback reports a failure or
    /// drops the frame without a `CMSampleBuffer`.
    #[must_use = "futures do nothing unless awaited"]
    #[allow(clippy::future_not_send)]
    pub fn encode_frame(
        &self,
        image_buffer: CVPixelBuffer,
        presentation_timestamp: CMTime,
        duration: CMTime,
        frame_properties: Option<CFDictionary>,
    ) -> impl Future<Output = Result<CMSampleBuffer, VTError>> + '_ {
        self.session.encode_frame_async(
            image_buffer,
            presentation_timestamp,
            duration,
            frame_properties,
        )
    }
}

#[cfg(feature = "compression")]
impl<'a> From<&'a CompressionSession> for AsyncCompressionSession<'a> {
    fn from(session: &'a CompressionSession) -> Self {
        Self::new(session)
    }
}

/// Async accessor for [`DecompressionSession`].
///
/// Wraps the decoder's one-shot frame completion callback as a future that can
/// be awaited on any executor.
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub struct AsyncDecompressionSession<'a> {
    session: &'a DecompressionSession,
}

impl core::fmt::Debug for AsyncDecompressionSession<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncDecompressionSession")
            .finish_non_exhaustive()
    }
}

impl<'a> AsyncDecompressionSession<'a> {
    /// Wrap a borrowed [`DecompressionSession`].
    #[must_use]
    pub const fn new(session: &'a DecompressionSession) -> Self {
        Self { session }
    }

    /// Submit `sample_buffer` for decoding and await the decoded image buffer.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncoderCallback`] if the decoder rejects the frame,
    /// reports an asynchronous failure, or completes without an image buffer.
    #[must_use = "futures do nothing unless awaited"]
    #[allow(clippy::future_not_send)]
    pub fn decode_frame(
        &self,
        sample_buffer: CMSampleBuffer,
        frame_flags: crate::ffi::VTDecodeFrameFlags,
    ) -> impl Future<Output = Result<CVImageBuffer, VTError>> + '_ {
        self.session.decode_frame_async(sample_buffer, frame_flags)
    }
}

impl<'a> From<&'a DecompressionSession> for AsyncDecompressionSession<'a> {
    fn from(session: &'a DecompressionSession) -> Self {
        Self::new(session)
    }
}

/// Async accessor for [`RawProcessingSession`].
#[cfg(feature = "frame_processor")]
#[cfg_attr(docsrs, doc(cfg(all(feature = "async", feature = "frame_processor"))))]
pub struct AsyncRawProcessingSession<'a> {
    session: &'a RawProcessingSession,
}

#[cfg(feature = "frame_processor")]
impl core::fmt::Debug for AsyncRawProcessingSession<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncRawProcessingSession")
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "frame_processor")]
impl<'a> AsyncRawProcessingSession<'a> {
    /// Wrap a borrowed [`RawProcessingSession`].
    #[must_use]
    pub const fn new(session: &'a RawProcessingSession) -> Self {
        Self { session }
    }

    /// Submit `input_pixel_buffer` for RAW processing and await the processed output.
    #[must_use = "futures do nothing unless awaited"]
    #[allow(clippy::future_not_send)]
    pub fn process_frame(
        &self,
        input_pixel_buffer: CVPixelBuffer,
    ) -> impl Future<Output = Result<CVPixelBuffer, VTError>> + '_ {
        self.session.process_frame_async(input_pixel_buffer)
    }

    /// Subscribe to RAW-parameter changes as a bounded async stream.
    ///
    /// Installing this stream replaces any existing parameter-change handler on
    /// the underlying session for the lifetime of the returned guard.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] when `VideoToolbox` rejects the handler
    /// install (for example on runtimes where the callback API is unavailable).
    pub fn parameter_changes(
        &self,
        capacity: usize,
    ) -> Result<RawProcessingParameterChangeStream<'a>, VTError> {
        RawProcessingParameterChangeStream::subscribe(self.session, capacity)
    }
}

#[cfg(feature = "frame_processor")]
impl<'a> From<&'a RawProcessingSession> for AsyncRawProcessingSession<'a> {
    fn from(session: &'a RawProcessingSession) -> Self {
        Self::new(session)
    }
}

/// Bounded async stream of updated RAW-processing parameter snapshots.
#[cfg(feature = "frame_processor")]
#[cfg_attr(docsrs, doc(cfg(all(feature = "async", feature = "frame_processor"))))]
pub struct RawProcessingParameterChangeStream<'a> {
    inner: BoundedAsyncStream<Vec<RawProcessingParameter>>,
    session: &'a RawProcessingSession,
}

#[cfg(feature = "frame_processor")]
impl RawProcessingParameterChangeStream<'_> {
    fn subscribe(
        session: &RawProcessingSession,
        capacity: usize,
    ) -> Result<RawProcessingParameterChangeStream<'_>, VTError> {
        let (stream, sender) = BoundedAsyncStream::new(capacity);
        session.set_parameter_changed_handler(move |parameters| {
            catch_user_panic(
                "videotoolbox::async_api::raw_processing_parameter_changes",
                || {
                    sender.push(parameters);
                },
            );
        })?;

        Ok(RawProcessingParameterChangeStream {
            inner: stream,
            session,
        })
    }

    /// Await the next buffered parameter-change notification.
    #[must_use]
    pub const fn next(&self) -> NextItem<'_, Vec<RawProcessingParameter>> {
        self.inner.next()
    }

    /// Try to pop the next buffered parameter-change notification without waiting.
    #[must_use]
    pub fn try_next(&self) -> Option<Vec<RawProcessingParameter>> {
        self.inner.try_next()
    }

    /// Number of buffered parameter-change notifications.
    #[must_use]
    pub fn buffered_count(&self) -> usize {
        self.inner.buffered_count()
    }
}

#[cfg(feature = "frame_processor")]
impl Drop for RawProcessingParameterChangeStream<'_> {
    fn drop(&mut self) {
        let _ = self.session.clear_parameter_changed_handler();
    }
}

#[cfg(feature = "frame_processor")]
impl core::fmt::Debug for RawProcessingParameterChangeStream<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawProcessingParameterChangeStream")
            .field("buffered_count", &self.buffered_count())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, feature = "frame_processor"))]
mod tests {
    use apple_cf::{cm::CMFormatDescription, cv::CVPixelBuffer};

    use super::AsyncRawProcessingSession;
    use crate::{copy_raw_processor_extension_properties, RawProcessingSession, VTError};

    const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

    #[test]
    fn async_raw_processing_process_frame_matches_existing_async_method_when_supported(
    ) -> Result<(), VTError> {
        pollster::block_on(async {
            let input = make_test_pixel_buffer(16, 16);
            let Ok(format) = make_video_format_description(&input) else {
                return Ok(());
            };

            if copy_raw_processor_extension_properties(&format).is_err() {
                return Ok(());
            }

            let session = match RawProcessingSession::new(&format) {
                Ok(session) => session,
                Err(VTError::SessionCreateFailed(_)) => return Ok(()),
                Err(error) => return Err(error),
            };

            let processed = match AsyncRawProcessingSession::new(&session)
                .process_frame(input)
                .await
            {
                Ok(processed) => processed,
                Err(VTError::EncodeFailed(_)) => return Ok(()),
                Err(error) => return Err(error),
            };

            assert_eq!(processed.width(), 16);
            assert_eq!(processed.height(), 16);
            Ok(())
        })
    }

    #[test]
    fn async_raw_processing_parameter_changes_constructs_stream_or_reports_runtime_gap(
    ) -> Result<(), VTError> {
        let input = make_test_pixel_buffer(16, 16);
        let Ok(format) = make_video_format_description(&input) else {
            return Ok(());
        };

        if copy_raw_processor_extension_properties(&format).is_err() {
            return Ok(());
        }

        let session = match RawProcessingSession::new(&format) {
            Ok(session) => session,
            Err(VTError::SessionCreateFailed(_)) => return Ok(()),
            Err(error) => return Err(error),
        };

        let result = match AsyncRawProcessingSession::new(&session).parameter_changes(4) {
            Ok(stream) => {
                assert_eq!(stream.buffered_count(), 0);
                assert!(stream.try_next().is_none());
                Ok(())
            }
            Err(VTError::ApiFailed { api, status }) => {
                assert_eq!(api, "VTRAWProcessingSessionSetParameterChangedHandler");
                assert_ne!(status, 0);
                Ok(())
            }
            Err(error) => Err(error),
        };
        result
    }

    fn make_test_pixel_buffer(width: usize, height: usize) -> CVPixelBuffer {
        CVPixelBuffer::create(width, height, BGRA)
            .unwrap_or_else(|status| panic!("CVPixelBuffer::create failed: {status}"))
    }

    fn make_video_format_description(
        pixel_buffer: &CVPixelBuffer,
    ) -> Result<CMFormatDescription, i32> {
        let mut description: apple_cf::raw::CMVideoFormatDescriptionRef = core::ptr::null();
        let status = unsafe {
            apple_cf::raw::CMVideoFormatDescriptionCreateForImageBuffer(
                apple_cf::raw::kCFAllocatorDefault,
                pixel_buffer.as_ptr().cast(),
                &raw mut description,
            )
        };
        if status == 0 && !description.is_null() {
            CMFormatDescription::from_raw(description.cast_mut().cast()).ok_or(status)
        } else {
            Err(status)
        }
    }
}
