//! `CoreMedia` primitives — `CMTime`, `CMSampleBuffer`, `CMBlockBuffer`,
//! `CMFormatDescription`, audio buffer list bridging.
//!
//! These are the framework-agnostic value types and reference-counted
//! wrappers shared by every Apple media framework. `ScreenCaptureKit` /
//! `VideoToolbox` / `AVFoundation` / `AVAssetWriter` all speak `CMSampleBuffer`,
//! and this module is the single source of truth for the safe Rust wrapper.
//!
//! The image-buffer accessor (`CMSampleBuffer::image_buffer`) is gated
//! behind the `cv` feature so users who only need `CMSampleBuffer` for
//! audio / encoded video don't pay the `CoreVideo` dependency cost.
//! The `SCStreamFrameInfo` attachment readers stay in `screencapturekit-rs`
//! since they're tied to `ScreenCaptureKit`'s specific attachment keys.

pub mod audio;
/// Core Media block-buffer wrappers.
pub mod block_buffer;
/// Core Media format-description wrappers.
pub mod format_description;
/// Core Media sample-buffer wrappers.
pub mod sample_buffer;
/// Core Media time and time-range wrappers.
pub mod time;
/// Core Media timebase wrappers.
pub mod timebase;

pub use audio::{AudioBuffer, AudioBufferList, AudioBufferListRaw};
pub use block_buffer::CMBlockBuffer;
pub use format_description::{CMFormatDescription, CMMetadataFormatDescription};
pub use sample_buffer::CMSampleBuffer;
pub use time::{CMClock, CMSampleTimingInfo, CMTime, CMTimeRange};
pub use timebase::CMTimebase;
