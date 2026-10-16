# apple-cf

Safe, dependency-free Rust bindings for Apple's shared **Core\*** frameworks — the foundation underneath the [doom-fish](https://github.com/doom-fish) macOS Rust suite.

> **Status:** `v0.7.1` tightens the safety documentation — every `unsafe impl Send/Sync` now carries a `// SAFETY:` justification comment, Drop implementations uniformly null-guard their release calls, and the `doom-fish-utils` version constraint follows the crate-family `>=X.Y, <X.(Y+2)` convention. See [`COVERAGE.md`](COVERAGE.md) for the framework summary and [`COVERAGE_AUDIT_V2.md`](COVERAGE_AUDIT_V2.md) for the full symbol audit.

## What's in the box

| Module | Framework | Feature flag | Status |
|---|---|---|---|
| [`cf`](src/cf) | CoreFoundation values, collections, property lists, locale/formatter helpers, runtime primitives | — | ✅ |
| [`raw`](src/raw) | Exhaustive low-level CoreFoundation/CoreMedia/CoreVideo/IOSurface/Dispatch bindings | — | ✅ |
| [`cg`](src/cg) | CoreGraphics value types + bitmap drawing wrappers | `cg` | ✅ |
| [`iosurface`](src/iosurface) | IOSurface (zero-copy GPU buffers, multi-planar formats) | `iosurface` | ✅ |
| [`dispatch_queue`](src/dispatch_queue.rs) | Dispatch queues, async/apply helpers, groups, semaphores, timer sources | `dispatch` | ✅ |
| [`cm`](src/cm) | `CMTime`, `CMTimeRange`, `CMTimebase`, `CMSampleBuffer`, `CMBlockBuffer`, `CMFormatDescription`, `CMMetadataFormatDescription` | `cm` | ✅ |
| [`cv`](src/cv) | `CVBuffer`, `CVImageBuffer`, `CVPixelBuffer`, `CVPixelBufferPool`, `CVMetalTextureCache` | `cv` | ✅ |
| [`utils`](src/utils) | Shared FFI helpers (always on) | — | ✅ |

## Why this crate exists

Every doom-fish crate that wraps a media-adjacent Apple framework needs the same primitives — `CFString`, `CGRect`, `CVPixelBuffer`, `IOSurface`, dispatch queues, time values. Instead of re-vendoring those inside every crate (and drifting), this crate owns them once.

```text
Safe Rust wrappers
    ├── CoreFoundation / Dispatch / CoreMedia / CoreVideo ergonomic APIs
    ├── exhaustive raw C bindings in `apple_cf::raw`
    ├── direct C FFI for value-only primitives where appropriate
    └── Swift @_cdecl bridge for reference-counted / callback-heavy surfaces
```

## Requirements

- macOS 13.0+
- Xcode toolchain (Swift 5.9+); Command Line Tools alone is **not** enough — `xcrun --sdk macosx --show-sdk-version` must succeed
- Apple Silicon or Intel Mac

## Installation

```toml
[dependencies]
apple-cf = "0.7"
```

Or pick only the frameworks you need:

```toml
[dependencies]
apple-cf = { version = ">=0.7, <0.9", default-features = false, features = ["cg", "cm", "cv", "dispatch", "iosurface"] }
```

## Quick examples

### Build CoreFoundation values and collections

```rust
use apple_cf::cf::{CFArray, CFString};

let first = CFString::new("first");
let second = CFString::new("second");
let array = CFArray::from_values(&[&first, &second]);
assert_eq!(array.len(), 2);
```

### Create a timer-backed dispatch source

```rust,no_run
use apple_cf::dispatch_queue::DispatchSource;
use std::thread;
use std::time::Duration;

let source = DispatchSource::timer(Duration::from_millis(5), Duration::from_millis(1));
source.resume();
thread::sleep(Duration::from_millis(20));
source.cancel();
assert!(source.fire_count() > 0);
```

### Allocate an IOSurface-backed `CVPixelBuffer`

```rust,no_run
use apple_cf::cv::CVPixelBuffer;

let pixel_buffer = CVPixelBuffer::create(16, 16, 0x4247_5241)
    .expect("pixel buffer");
assert_eq!(pixel_buffer.width(), 16);
```

## Architecture

This crate uses the same Swift-bridge pattern as the rest of the doom-fish crates:

- `swift-bridge/Sources/<Framework>Bridge/` exposes thin `@_cdecl` entry points
- `src/ffi/*.rs` declares the matching `extern "C"` bindings
- `src/<framework>/` provides the safe Rust API on top

The Rust crate has **zero runtime dependencies**.

## Examples and tests

This release ships 15 numbered examples plus dedicated smoke tests for:

- CoreFoundation primitives, collections, property lists, resources, runtime helpers
- Dispatch queues, `dispatch_async`, `dispatch_async_and_wait`, `dispatch_apply`, groups, semaphores, timer sources, and raw main-queue access
- `CMTimeRange`, `CMTimebase`, `CMMetadataFormatDescription`, and low-level `CMTag`/`CMSync` coverage through `apple_cf::raw`
- `CVBuffer`, `CVImageBuffer`, `CVPixelBuffer`, `CVMetalTextureCache`, and the remaining CVMetal entry points through `apple_cf::raw`
- Exhaustive low-level constants / inline helpers surfaced by `apple_cf::raw`

`CVDisplayLink` remains exempt in the audit because Apple deprecated the family on macOS 15.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
