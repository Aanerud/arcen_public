# Changelog

## [0.9.3] - 2026-05-20

- Clippy hygiene sweep: cleared all `-D warnings` lints across the crate. No public API change.

## [0.9.2] - 2026-05-20

- Widen `doom-fish-utils` dependency bound to `<0.4` so the 0.3.x SPSC-ring release resolves cleanly across the fleet. No source changes.

## [0.9.1] - 2026-05-19

- Bump MSRV from 1.70 to 1.76 to match fleet baseline.

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-05-18

### Changed

- Added the last three missing CoreGraphics raw aliases for downstream re-export and dedup work: `CGContextRef`, `CGCharCode`, and `CGKeyCode`.

## [0.8.1] - 2026-05-18

### Changed

- Added `Debug` coverage to the remaining public structs that can safely expose it in `apple-cf`: 5 structs touched total, with `CFPropertyList`, `CFPreferences`, and `CFXML` now deriving `Debug`, plus manual pointer-aware `Debug` implementations for `CGColorSpace` and `CGImage`.

## [0.8.0] - 2026-05-18

### Changed

- **BREAKING**: `CGRect` now matches Apple's `CGGeometry.h` definition with nested `origin: CGPoint` and `size: CGSize` fields. The previous flat `{ x, y, width, height }` layout had the same byte ordering, so FFI compatibility is preserved, but field access must change: `rect.x` → `rect.origin.x`, `rect.y` → `rect.origin.y`, `rect.width` → `rect.size.width`, and `rect.height` → `rect.size.height`.
- `CGRect::new(x, y, width, height)` is unchanged and still takes four `f64` values.
- Added `CGRect::from_origin_size(origin, size)`.
- Added `CGRect::is_empty()` and `CGRect::contains_point()`.

## [0.7.2] - 2026-05-18

### Changed

- Added one-line `///` documentation across the raw bindings, bridge FFI surface, and safe wrappers, raising measured public-item coverage to 100.0% (5,904 of 5,904 items documented).
- Clarified ownership expectations for raw-pointer constructors and added `# Safety` sections to public `unsafe fn` items where needed.
- Bumped `Cargo.toml` from `0.7.1` to `0.7.2` for this documentation-only release with no API or ABI changes.

## [0.7.1] — 2026-05-20

### Changed

- Added `// SAFETY:` justification comments to every bare `unsafe impl Send`
  and `unsafe impl Sync` declaration across `iosurface`, `cv`, `cm`, `cg`, and
  `dispatch_queue` modules.  All Apple Core Foundation / Core Media / Core Video
  / GCD opaque-pointer types are documented as thread-safe by Apple; the
  comments now make this contract explicit in source.
- Normalised `Drop` implementations: `IOSurface`, `CVPixelBuffer`,
  `CVPixelBufferPool`, and `CVMetalTextureCache` now null-guard their release
  calls, matching the convention already in place for `CFType` and
  `CMSampleBuffer`.
- Updated `doom-fish-utils` version constraint from `"0.1"` to `">=0.1, <0.3"`
  following the crate-family `>=X.Y, <X.(Y+2)` convention.
- README installation snippet updated to reflect the current `0.7` line.

## [0.7.0] — 2026-05-17

### Changed

- **Factored framework-agnostic helpers into the new
  [`doom-fish-utils`](https://crates.io/crates/doom-fish-utils) crate**:
  `completion`, `ffi_string`, `four_char_code`, and `panic_safe` now
  live in `doom-fish-utils` so any doom-fish family crate can pull them
  in without depending on the full `apple-cf` Core* surface.
- `apple_cf::utils` is preserved as a re-export shim, so downstream
  call sites (`apple_cf::utils::{completion, ffi_string,
  four_char_code, FourCharCode, panic_safe}`) keep compiling without
  any changes. The string-owning helpers
  (`ffi_string_owned`, `ffi_string_owned_or_empty`) remain
  apple-cf-specific shims that bake in `acf_free_string` as the
  deallocator; the underlying generic helpers in `doom-fish-utils`
  take a caller-supplied `free_fn` for sibling crates that need a
  different `_free_string` symbol.
- Added `doom-fish-utils = { version = "0.1" }` dependency.
- `Cargo.toml` version bumped to `0.7.0` (minor: dependency graph
  change, no public API breakage).
- `COVERAGE_AUDIT_V2.md` published — independent re-verification
  against `MacOSX26.2.sdk` confirming 100% non-exempt coverage of
  the sampled top-300 symbols per framework.

### Removed

- `apple_cf/src/utils/{completion.rs, four_char_code.rs,
  panic_safe.rs}` — relocated to `doom-fish-utils`. The
  `apple_cf::utils::*` import paths still resolve via re-exports.

## [0.6.2]

### Added

- New `raw` module with exhaustive low-level CoreFoundation / CoreMedia / CoreVideo / IOSurface / Dispatch bindings generated from the active macOS SDK, plus hand-written coverage for inline helpers like `CFByteOrder*`, `CFString*InlineBuffer`, `CMTag*`, `CMTimebase*` compatibility shims, `dispatch_get_main_queue`, and the remaining CVMetal declarations.
- Smoke example `15_raw_bindings` and matching `raw_bindings_tests` coverage harness for the new exhaustive raw surface.

### Changed

- `COVERAGE_AUDIT.md` now reports `0` remaining gaps and `100.00%` coverable coverage (`95.18%` overall once deprecated / unavailable exemptions are included).
- README / COVERAGE docs refreshed for the new `apple_cf::raw` module.
- Dispatch queue creation bridge renamed to `acf_dispatch_queue_create` to avoid colliding with the system `dispatch_queue_create` symbol now exposed through `apple_cf::raw`.
- `Cargo.toml` version bumped to `0.6.2`.

## [0.6.1]

### Added

- `cf::CFSet` / `CFMutableSet` and `cf::CFPropertyList`, including Swift bridge coverage, examples, and smoke tests.
- `dispatch_queue::dispatch_async`, `dispatch_async_and_wait`, and `dispatch_apply` safe helpers.
- `cm::CMMetadataFormatDescription` plus metadata-description constants, constructors, identifier lookup, and merge/extend helpers.
- New numbered example `14_cm_metadata_format_description` and matching CoreMedia metadata smoke tests.

### Changed

- `COVERAGE_AUDIT.md` refreshed for the highest-value remaining gaps; deprecated `CVDisplayLink` symbols now live in the exempt bucket.
- README / COVERAGE docs refreshed for the new CoreFoundation, Dispatch, and CoreMedia surface.
- `Cargo.toml` version bumped to `0.6.1`.

## [0.6.0]

### Added

- **`cf` module** — safe Core Foundation wrappers for:
  - value types: `CFType`, `CFString`, `CFNumber`, `CFData`, `CFDate`, `CFUUID`, `CFError`
  - collections: `CFArray`, `CFDictionary` / `CFDict`, `CFBag`, `CFTree`, `CFAttributedString`
  - resources / locale / formatting: `CFURL`, `CFBundle`, `CFLocale`, `CFCalendar`, `CFTimeZone`, `CFCharacterSet`, `CFNumberFormatter`, `CFDateFormatter`, `CFPreferences`, `CFFileSecurity`, `CFXML`
  - runtime helpers: `CFNotificationCenter`, `CFRunLoop`, `CFTimer`, `CFMessagePort`, `CFStreamPair`, `CFSocket`, `CFFileDescriptor`
- **Dispatch sync primitives** — `DispatchGroup`, `DispatchSemaphore`, and timer-backed `DispatchSource` in `dispatch_queue`.
- **CoreMedia time extras** — `CMTimeRange`, `CMClock::host_time_clock()`, and `CMTimebase`.
- **CoreVideo extras** — `CVBuffer`, `CVImageBuffer`, `CVMetalTextureCache`.
- Eight new numbered examples (`06_` through `13_`) covering the new CoreFoundation / Dispatch / CoreMedia / CoreVideo surface.
- Seven new test files covering the new wrappers.
- `COVERAGE.md` header-audit summary for the Wave-C sweep.

### Changed

- README refreshed for the expanded CoreFoundation / Dispatch / media coverage.
- `Cargo.toml` version bumped to `0.6.0`.
- `build.rs` now links the `Metal` framework for `CVMetalTextureCache` support.

## [0.5.0]

### Added

- **`cg::CGContext`** — safe Core Graphics bitmap-context wrapper with RGBA8 and grayscale constructors, byte accessors, path/rect drawing, transforms, graphics-state save/restore, image drawing, and bitmap snapshots.
- `CGImage::save_png()` helper backed by the existing ImageIO Swift bridge so bitmap snapshots can be written to disk without extra dependencies.
- Smoke example `05_cgcontext_smoke` proving a 64×64 offscreen `CGContext` can draw shapes, snapshot to `CGImage`, export a PNG, and verify pixel contents.

### Changed

- `cg` module docs now cover both value types and bitmap drawing wrappers.

### Added

- **`cv` module** — `CVPixelBuffer` and `CVPixelBufferPool` carved out of
  `screencapturekit-rs`. Wraps the CoreVideo primitives that pair an
  IOSurface with format metadata.
- `cv` feature flag (on by default; implies `iosurface`).
- `CoreVideoBridge` Swift target with the underlying `cv_pixel_buffer_*`
  and `cv_pixel_buffer_pool_*` `@_cdecl` exports.
- `CVPixelBuffer::create_with_io_surface(&IOSurface)` lets downstream
  consumers (e.g. `vision-rs`) ingest live capture data without a PNG
  round-trip.
- Smoke test `04_cv_pixel_buffer` proves the IOSurface ↔ CVPixelBuffer
  round-trip: write `[0xDE, 0xAD, 0xBE, 0xEF]` via IOSurface, read back
  via the wrapped CVPixelBuffer, verify identical bytes and identical
  IOSurface id on the round-trip.
- API harness extended to CVPixelBuffer + CVPixelBufferPool — 7/7 tests
  pass at 100% coverable.

### Added

- **`cm` module** — CoreMedia value types and reference-counted wrappers
  carved out of `screencapturekit-rs`:
  - `CMTime` / `CMSampleTimingInfo` (pure value types, 0 deps)
  - `CMSampleBuffer` — safe Drop/Clone wrapper with accessors for PTS, DTS,
    duration, num_samples, validity, format description, data buffer, and
    raw image-buffer pointer hand-off. SCStreamFrameInfo attachment readers
    intentionally **not** ported — those stay in screencapturekit-rs.
  - `CMBlockBuffer` — Drop/Clone wrapper with data length, contiguous-range
    check, byte-copy, data pointer access, and create-with-data / create-empty
    constructors.
  - `CMFormatDescription` — Drop/Clone wrapper with media type / subtype /
    extensions, plus audio-specific accessors (sample rate, channel count,
    bits-per-channel, bytes-per-frame, format flags).
  - `audio` — `AudioBuffer` / `AudioBufferList` / `AudioBufferListRaw`
    bridging types ported verbatim.
- **CoreMediaBridge Swift target** with 28 `@_cdecl` exports covering the
  generic CMSampleBuffer / CMBlockBuffer / CMFormatDescription surface.
- **`cm` feature flag** (on by default) so audio-only consumers can opt
  out of the CoreMedia symbols.
- Smoke test `03_cm_sample_buffer` proves end-to-end retain/release across
  the videotoolbox ↔ apple-cf boundary: encodes one H.264 frame, wraps the
  resulting CMSampleBuffer in our safe type, and inspects PTS/data-buffer/
  format-description with real values (`vide` / `avc1`, 142 bytes of H.264).
- API coverage harness extended to CMSampleBuffer / CMBlockBuffer /
  CMFormatDescription — 5/5 tests pass at 100% coverable coverage.

### Changed

- Re-exports from `prelude`: `CMTime`, `CMSampleBuffer`, `CMBlockBuffer`,
  `CMFormatDescription` join the ergonomic prelude (gated on `cm` feature).


### Added

- Initial scaffold carved out of `screencapturekit-rs`.
- `cg` — CoreGraphics value types (`CGRect`, `CGPoint`, `CGSize`).
- `iosurface` — full `IOSurface` API (single- and multi-planar, lock/unlock,
  use-count tracking, properties).
- `dispatch_queue` — `DispatchQueue` + `DispatchQoS`.
- `utils` — `FourCharCode`, `SyncCompletion` / `AsyncCompletion`,
  `ffi_string_owned`, `panic_safe` callback wrapper.
- Swift bridge with separate `CoreGraphicsBridge`, `IOSurfaceBridge`,
  `DispatchBridge` targets aggregated under a single static
  `AppleCFBridge` library.
- `acf_free_string` centralised heap-string deallocator.
- Two smoke-test examples that exercise the full Rust → C FFI → Swift →
  Apple framework path.

### Planned

- `cm` (CoreMedia) once `SCStreamFrameInfo` attachments are decoupled
  upstream in screencapturekit-rs.
- `cv` (CoreVideo).
- `metal` (Metal).
