# videotoolbox

Safe Rust bindings for Apple's [VideoToolbox](https://developer.apple.com/documentation/videotoolbox) framework — hardware-accelerated encode/decode, pixel transfer/rotation, multipass helpers, HDR metadata, motion estimation, RAW processing, and `VTFrameProcessor` pipelines on macOS.

> **Status:** experimental, but the crate now covers the main public `VideoToolbox` surfaces used by the doom-fish stack. Objective-C-only APIs use a small Swift bridge behind the `frame_processor` feature, and executor-agnostic encode/decode/RAW-processing async helpers live in `videotoolbox::async_api` behind the `async` feature.

## Features

- **Hardware-accelerated encoding + decoding** — H.264, HEVC, and `ProRes` 422/4444
- **Pixel transfer / rotation / utilities** — `VTPixelTransferSession`, `VTPixelRotationSession`, `VTCreateCGImageFromCVPixelBuffer`
- **Multipass + HDR helpers** — `VTFrameSilo`, `VTMultiPassStorage`, `VTHDRPerFrameMetadataGenerationSession`
- **Advanced processing** — `VTFrameProcessor`, `VTMotionEstimationSession`, `VTRAWProcessingSession`
- **Direct `IOSurface` input/output** — zero-copy composition with [`apple-cf::iosurface`](https://github.com/doom-fish/apple-cf-rs)
- **Builder pattern** — fluent encoder configuration for bitrate, frame rate, keyframe interval, real-time mode, and profile level
- **Executor-agnostic async module** — `videotoolbox::async_api::{AsyncCompressionSession, AsyncDecompressionSession, AsyncRawProcessingSession}` bridges one-shot frame callbacks to `Future`s and wraps RAW-parameter change notifications as a bounded async stream via `doom-fish-utils`
- **Mostly pure C bindings** — optional Swift bridge only for Objective-C-only APIs
- **Minimal dependencies** — [`apple-cf`](https://github.com/doom-fish/apple-cf-rs), plus optional [`apple-metal`](https://github.com/doom-fish/apple-metal-rs) for `VTFrameProcessor` command-buffer integration

### Async notes

- `AsyncRawProcessingSession::parameter_changes(...)` exposes `VTRAWProcessingSessionSetParameterChangedHandler` as a bounded async stream.
- `VTDecompressionSessionSetMultiImageCallback` remains sync-only for now: the audited C API requires a non-null callback and exposes no clear / unsubscribe hook for an RAII async stream wrapper.

## Why not bindgen?

The full `VideoToolbox` SDK surface is large, but the useful set for real macOS media pipelines is still small enough to hand-audit. Hand-writing those declarations gives us:

- No build-time dependency on `clang`
- Type-safe Rust enums for codec types (instead of raw `u32` four-character codes)
- Builder APIs that map ergonomically to VT's `CFDictionary` property bag

## Requirements

- macOS 13.0+
- Apple Silicon or Intel Mac with hardware video encoder

## Quick start

```rust,no_run
use videotoolbox::prelude::*;
use apple_cf::iosurface::{IOSurface, IOSurfaceLockOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Allocate a 1920×1080 BGRA IOSurface.
    let surface = IOSurface::create(1920, 1080, u32::from_be_bytes(*b"BGRA"), 4)
        .ok_or("failed to allocate")?;

    // Build a real-time H.264 encoder.
    let encoder = CompressionSession::builder(1920, 1080, Codec::H264)
        .with_real_time(true)
        .with_average_bit_rate(8_000_000)
        .with_expected_frame_rate(60.0)
        .with_max_keyframe_interval(120)
        .build()?;

    // Encode one frame and inspect the resulting CMSampleBuffer.
    let encoded = encoder.encode(&surface, (0, 60))?;
    println!("Got {} bytes of H.264", encoded.data.len());

    if let Some(sb) = encoded.cm_sample_buffer() {
        // Hand `sb` straight to avassetwriter::Writer::append_sample for
        // zero-copy muxing — no raw pointer hand-off needed.
        let _ = sb.is_valid();
    }

    Ok(())
}
```

## Composes with the rest of the doom-fish stack

```text
screencapturekit-rs ──► IOSurface ──► videotoolbox-rs ──► H.264 bytes
                                              ↓
                                        avassetwriter-rs (future)
                                              ↓
                                          .mp4 file
```

## Roadmap

- [x] `VTCompressionSession` (encoder)
- [x] `VTDecompressionSession` (decoder)
- [x] `VTPixelTransferSession` (pixel format / colour space conversion)
- [x] `VTPixelRotationSession`
- [x] `VTMultiPassStorage` + `VTFrameSilo` (two-pass encoding)
- [x] `VTHDRPerFrameMetadataGenerationSession` (Dolby Vision metadata)
- [x] `VTFrameProcessor` capability queries (super-resolution / optical flow detection)
- [x] `VTFrameProcessor` pipeline (super-resolution + motion blur + temporal noise + frame-rate conversion + optical flow + 2 low-latency variants)
- [x] `VTMotionEstimationSession`
- [x] `VTRAWProcessingSession` (with parameter introspection)
- [x] `VTProfessionalVideoWorkflow` decoder/encoder registration
- [x] `VTCreateCGImageFromCVPixelBuffer`
- [x] HEVC profile-level helpers
- [x] Executor-agnostic async encode/decode/RAW-processing module behind the `async` feature

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
