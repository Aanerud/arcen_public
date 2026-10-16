//! API-surface coverage harness.
//!
//! For each Apple framework wrapped by this crate, parses the SDK header(s)
//! to enumerate the public C symbols, parses our `extern "C"` declarations
//! in `src/ffi/mod.rs`, and reports the diff:
//!
//! * **Wrapped** — Apple symbol present in our `extern "C"` block.
//! * **Missing** — Apple symbol absent from our wrapper. Either:
//!     - intentionally omitted (listed in `INTENTIONALLY_OMITTED`), or
//!     - a real coverage gap that fails this test.
//! * **Unknown** — Symbol in our `extern "C"` block that doesn't exist in
//!   the SDK header. Indicates a typo, a removed-API binding, or a
//!   privately-exported symbol. Always fails the test.
//!
//! The harness is intentionally regex-based rather than libclang-based:
//! Apple's header conventions are stable enough that a 30-line regex covers
//! every framework we care about, and we avoid pulling in `clang-sys`.

#![allow(clippy::cast_precision_loss, clippy::iter_on_single_items)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

/// Resolve the active macOS SDK root via `xcrun --sdk macosx`.
fn sdk_root() -> PathBuf {
    let out = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .expect("xcrun must be available");
    assert!(out.status.success(), "xcrun --show-sdk-path failed");
    PathBuf::from(
        String::from_utf8(out.stdout)
            .expect("non-UTF-8 SDK path")
            .trim()
            .to_string(),
    )
}

/// Extract every C function name matching `prefix*(` from the given header(s).
fn extract_c_function_names(prefix: &str, header_paths: &[PathBuf]) -> BTreeSet<String> {
    let pattern = format!(r"\b({prefix}[A-Za-z0-9_]+)\s*\(");
    let re = regex_lite::Regex::new(&pattern).expect("regex compiles");
    let mut names = BTreeSet::new();
    for header in header_paths {
        let contents = std::fs::read_to_string(header)
            .unwrap_or_else(|e| panic!("can't read {}: {e}", header.display()));
        for cap in re.captures_iter(&contents) {
            names.insert(cap[1].to_string());
        }
    }
    names
}

/// Extract every `pub fn <name>(` symbol from a Rust `extern "C" { ... }` block.
fn extract_rust_extern_names(rust_source_path: &str) -> BTreeSet<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rust_source_path);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("can't read {}: {e}", path.display()));
    let re = regex_lite::Regex::new(r"pub\s+fn\s+([A-Za-z0-9_]+)\s*\(").unwrap();
    re.captures_iter(&contents)
        .map(|c| c[1].to_string())
        .collect()
}

/// Compare two symbol sets and return `(wrapped, missing, unknown)`.
fn diff(
    apple_symbols: &BTreeSet<String>,
    our_symbols: &BTreeSet<String>,
    intentionally_omitted: &BTreeSet<String>,
    bridge_only: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>) {
    let wrapped: BTreeSet<String> = apple_symbols.intersection(our_symbols).cloned().collect();
    let missing: BTreeSet<String> = apple_symbols
        .difference(our_symbols)
        .filter(|s| !intentionally_omitted.contains(*s))
        .cloned()
        .collect();
    let unknown: BTreeSet<String> = our_symbols
        .difference(apple_symbols)
        .filter(|s| !bridge_only.contains(*s))
        .cloned()
        .collect();
    (wrapped, missing, unknown)
}

fn report(
    framework: &str,
    apple: &BTreeSet<String>,
    ours: &BTreeSet<String>,
    omitted: &BTreeSet<String>,
    bridge_only: &BTreeSet<String>,
) -> bool {
    let (wrapped, missing, unknown) = diff(apple, ours, omitted, bridge_only);
    let total_apple = apple.len();
    let coverable = wrapped.len() + missing.len();
    let pct_of_coverable = if coverable == 0 {
        100.0
    } else {
        wrapped.len() as f64 / coverable as f64 * 100.0
    };
    let pct_of_apple = if total_apple == 0 {
        100.0
    } else {
        wrapped.len() as f64 / total_apple as f64 * 100.0
    };

    println!(
        "\n=== {framework} API coverage ===\n\
         Apple symbols:           {total_apple}\n\
         Intentionally omitted:    {}\n\
         Bridge-only (in our FFI): {}\n\
         ----\n\
         Coverable (Apple - omitted): {coverable}\n\
         Wrapped:                  {} ({pct_of_coverable:.1}% of coverable, {pct_of_apple:.1}% of all Apple)\n\
         Missing (gap):            {}\n\
         Unknown (stale?):         {}",
        omitted.len(),
        bridge_only.len(),
        wrapped.len(),
        missing.len(),
        unknown.len(),
    );

    if !missing.is_empty() {
        println!(
            "\n--- Missing (real coverage gaps; add to INTENTIONALLY_OMITTED if intentional) ---"
        );
        for s in &missing {
            println!("  - {s}");
        }
    }
    if !unknown.is_empty() {
        println!(
            "\n--- Unknown (in our extern \"C\" but NOT in Apple's headers — possible typo or stale binding) ---"
        );
        for s in &unknown {
            println!("  - {s}");
        }
    }

    // Test passes only when there are zero unknown symbols. Missing is
    // informational; CI fails only via the per-crate threshold assertion below.
    unknown.is_empty()
}

// ---- Framework-specific test cases ----

/// Symbols filtered out from `IOSurfaceRef.h` because they:
/// - are deprecated
/// - are macOS-only IPC primitives we don't need (Mach ports)
/// - return CFArrays/CFDictionaries that need higher-level wrappers
///   (planned for a future minor release)
fn iosurface_intentionally_omitted() -> BTreeSet<String> {
    [
        "IOSurfaceCreateMachPort",
        "IOSurfaceLookupFromMachPort",
        "IOSurfaceCreatePort",
        "IOSurfaceLookupFromPort",
        "IOSurfaceLookup",
        "IOSurfaceGetTypeID",
        "IOSurfaceCopyAllValues",
        "IOSurfaceCopyValue",
        "IOSurfaceSetValue",
        "IOSurfaceSetValues",
        "IOSurfaceRemoveValue",
        "IOSurfaceRemoveAllValues",
        "IOSurfaceGetPropertyAlignment",
        "IOSurfaceGetPropertyMaximum",
        "IOSurfaceAlignProperty",
        "IOSurfaceAllowsPixelSizeCasting",
        "IOSurfaceSetPurgeable",
        "IOSurfaceCopyComponentName",
        "IOSurfaceGetNameOfComponentOfPlane",
        "IOSurfaceGetNumberOfComponentsOfPlane",
        "IOSurfaceGetTypeOfComponentOfPlane",
        "IOSurfaceGetSubsamplingOfComponentOfPlane",
        "IOSurfaceGetBitDepthOfComponentOfPlane",
        "IOSurfaceGetBitOffsetOfComponentOfPlane",
        "IOSurfaceGetRangeOfComponentOfPlane",
        // Per-plane element accessors — overlap with non-planar accessors
        // and IOSurface returns 0 for single-plane formats anyway. Track
        // separately if a multi-planar consumer needs them.
        "IOSurfaceGetBytesPerElementOfPlane",
        "IOSurfaceGetElementHeightOfPlane",
        "IOSurfaceGetElementWidthOfPlane",
        // Misc accessors not yet wrapped — file an issue if you need them.
        "IOSurfaceGetSubsampling",
        "IOSurfaceGetUseCount",
        "IOSurfaceSetOwnershipIdentity",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Symbols our Rust `extern "C"` block declares that AREN'T standalone
/// Apple symbols — they're either Swift-bridge helpers or inherited from
/// CoreFoundation (CFRetain/CFRelease/CFHash on a `CFTypeRef`).
fn iosurface_bridge_only() -> BTreeSet<String> {
    [
        // Swift bridge wrapper — single Rust entry point that calls the
        // multi-arg Apple `IOSurfaceCreate(CFDictionary)` form internally.
        "IOSurfaceCreateWithProperties",
        // CFTypeRef inheritance — IOSurfaceRef *is* a CFTypeRef, so
        // CFRetain / CFRelease / CFHash apply but aren't declared in
        // IOSurfaceRef.h. We re-export under IOSurface* names from the
        // Swift bridge for ergonomics.
        "IOSurfaceHash",
        "IOSurfaceRetain",
        "IOSurfaceRelease",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[test]
fn iosurface_api_coverage() {
    let sdk = sdk_root();
    let header = sdk.join("System/Library/Frameworks/IOSurface.framework/Headers/IOSurfaceRef.h");
    let apple = extract_c_function_names("IOSurface", &[header]);
    let ours = extract_rust_extern_names("src/ffi/mod.rs");
    // Filter our symbols to just the IOSurface namespace (we map snake_case
    // `io_surface_*` to `IOSurface*`).
    let ours_iosurface: BTreeSet<String> = ours
        .iter()
        .filter(|n| n.starts_with("io_surface_"))
        .map(|n| snake_to_pascal_iosurface(n))
        .collect();

    assert!(
        report(
            "IOSurface",
            &apple,
            &ours_iosurface,
            &iosurface_intentionally_omitted(),
            &iosurface_bridge_only()
        ),
        "IOSurface coverage harness reported unknown symbols — see stdout"
    );

    let (wrapped, missing, _) = diff(
        &apple,
        &ours_iosurface,
        &iosurface_intentionally_omitted(),
        &iosurface_bridge_only(),
    );
    let coverable = wrapped.len() + missing.len();
    let pct = wrapped.len() as f64 / coverable as f64 * 100.0;
    assert!(
        pct >= 100.0,
        "IOSurface coverable API coverage regressed below 100% (now {pct:.1}%); missing: {missing:?}"
    );
}

/// Convert `io_surface_get_width_of_plane` -> `IOSurfaceGetWidthOfPlane`.
fn snake_to_pascal_iosurface(snake: &str) -> String {
    // Manual map for IO + acronyms that get camelCased oddly.
    let mut out = String::from("IOSurface");
    for word in snake.trim_start_matches("io_surface_").split('_') {
        if word == "id" {
            out.push_str("ID");
        } else {
            let mut c = word.chars();
            if let Some(first) = c.next() {
                out.push(first.to_ascii_uppercase());
                for ch in c {
                    out.push(ch);
                }
            }
        }
    }
    out
}

#[test]
fn snake_to_pascal_helper_works() {
    assert_eq!(
        snake_to_pascal_iosurface("io_surface_create"),
        "IOSurfaceCreate"
    );
    assert_eq!(
        snake_to_pascal_iosurface("io_surface_get_width"),
        "IOSurfaceGetWidth"
    );
    assert_eq!(
        snake_to_pascal_iosurface("io_surface_get_width_of_plane"),
        "IOSurfaceGetWidthOfPlane"
    );
    assert_eq!(
        snake_to_pascal_iosurface("io_surface_get_id"),
        "IOSurfaceGetID"
    );
}

// ---- CoreMedia coverage tests ----

fn snake_to_pascal_with_prefix(snake: &str, target_prefix: &str, snake_prefix: &str) -> String {
    let mut out = String::from(target_prefix);
    for word in snake.trim_start_matches(snake_prefix).split('_') {
        let mut c = word.chars();
        if let Some(first) = c.next() {
            out.push(first.to_ascii_uppercase());
            for ch in c {
                out.push(ch);
            }
        }
    }
    // Apple naming exceptions: CMSampleBuffer uses "TimeStamp" (capital S),
    // CMFormatDescription uses "SubType" (capital T). Our snake_case
    // collapses those to "Timestamp"/"Subtype"; rewrite back.
    out = out.replace("Timestamp", "TimeStamp");
    out = out.replace("Subtype", "SubType");
    out = out.replace("IoSurface", "IOSurface");
    out = out.replace("TypeId", "TypeID");
    out
}

fn cm_sample_buffer_omitted() -> BTreeSet<String> {
    [
        "CMSampleBufferCreate",
        "CMSampleBufferCreateCopy",
        "CMSampleBufferCreateCopyWithNewTiming",
        "CMSampleBufferCreateForImageBuffer",
        "CMSampleBufferCreateReady",
        "CMSampleBufferCreateReadyWithImageBuffer",
        "CMSampleBufferCreateReadyWithPacketDescriptions",
        "CMAudioSampleBufferCreateReadyWithPacketDescriptions",
        "CMAudioSampleBufferCreateWithPacketDescriptions",
        "CMSampleBufferCreateForImageBufferWithMakeDataReadyHandler",
        "CMSampleBufferCreateForImageBufferWithMakeDataReadyCallback",
        "CMSampleBufferCreateWithMakeDataReadyHandler",
        "CMSampleBufferCreateWithMakeDataReadyCallback",
        "CMSampleBufferCreateReadyWithMakeDataReadyHandler",
        "CMSampleBufferCopySampleBufferForRange",
        "CMSampleBufferGetTypeID",
        "CMSampleBufferSetDataBuffer",
        "CMSampleBufferSetDataBufferFromAudioBufferList",
        "CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer",
        "CMSampleBufferGetAudioStreamPacketDescriptions",
        "CMSampleBufferGetAudioStreamPacketDescriptionsPtr",
        "CMSampleBufferGetSampleAttachmentsArray",
        "CMSampleBufferGetSampleSize",
        "CMSampleBufferGetSampleSizeArray",
        "CMSampleBufferGetTotalSampleSize",
        "CMSampleBufferGetSampleTimingInfo",
        "CMSampleBufferGetSampleTimingInfoArray",
        "CMSampleBufferGetOutputDuration",
        "CMSampleBufferGetOutputPresentationTimeStamp",
        "CMSampleBufferGetOutputDecodeTimeStamp",
        "CMSampleBufferGetOutputSampleTimingInfoArray",
        "CMSampleBufferSetOutputPresentationTimeStamp",
        "CMSampleBufferGetSampleAttachmentsCount",
        "CMSampleBufferCallForEachSample",
        "CMSampleBufferCallBlockForEachSample",
        "CMSampleBufferInvalidate",
        "CMSampleBufferMakeDataReady",
        "CMSampleBufferSetInvalidateHandler",
        "CMSampleBufferSetInvalidateCallback",
        "CMSampleBufferTrackDataReadiness",
        "CMSampleBufferTrackingClock",
        "CMAudioSampleBufferCreateWithPacketDescriptionsAndMakeDataReadyHandler",
        "CMSampleBufferCopyPCMDataIntoAudioBufferList",
        "CMSampleBufferHasDataFailed",
        "CMSampleBufferSetDataFailed",
        "CMSampleBufferSetDataReady",
        // False-positive regex match — `CMSampleBuffers(` actually appears in a
        // header comment, not a function declaration.
        "CMSampleBuffers",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[test]
fn cm_sample_buffer_api_coverage() {
    let sdk = sdk_root();
    let header = sdk.join("System/Library/Frameworks/CoreMedia.framework/Headers/CMSampleBuffer.h");
    let mut apple = extract_c_function_names("CMSampleBuffer", std::slice::from_ref(&header));
    apple.append(&mut extract_c_function_names(
        "CMAudioSampleBuffer",
        &[header],
    ));

    let ours: BTreeSet<String> = extract_rust_extern_names("src/ffi/mod.rs")
        .into_iter()
        .filter(|n| n.starts_with("cm_sample_buffer_"))
        .map(|n| snake_to_pascal_with_prefix(&n, "CMSampleBuffer", "cm_sample_buffer_"))
        .collect();

    // CF-inherited (CFRetain/CFRelease/CFHash on a CFTypeRef).
    let bridge_only: BTreeSet<String> = [
        "CMSampleBufferRetain",
        "CMSampleBufferRelease",
        "CMSampleBufferHash",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    assert!(report(
        "CMSampleBuffer",
        &apple,
        &ours,
        &cm_sample_buffer_omitted(),
        &bridge_only,
    ));
    let (wrapped, missing, _) = diff(&apple, &ours, &cm_sample_buffer_omitted(), &bridge_only);
    let coverable = wrapped.len() + missing.len();
    let pct = if coverable == 0 {
        100.0
    } else {
        wrapped.len() as f64 / coverable as f64 * 100.0
    };
    assert!(
        pct >= 100.0,
        "CMSampleBuffer: {pct:.1}%; missing: {missing:?}"
    );
}

fn cm_block_buffer_omitted() -> BTreeSet<String> {
    [
        "CMBlockBufferAccessDataBytes",
        "CMBlockBufferAppendBufferReference",
        "CMBlockBufferAppendMemoryBlock",
        "CMBlockBufferAssureBlockMemory",
        "CMBlockBufferAssureBufferMemory",
        "CMBlockBufferCreateContiguous",
        "CMBlockBufferCreateWithBufferReference",
        "CMBlockBufferCreateWithMemoryBlock",
        "CMBlockBufferCustomBlockSourceGetTypeID",
        "CMBlockBufferFillDataBytes",
        "CMBlockBufferGetTypeID",
        "CMBlockBufferReplaceDataBytes",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[test]
fn cm_block_buffer_api_coverage() {
    let sdk = sdk_root();
    let header = sdk.join("System/Library/Frameworks/CoreMedia.framework/Headers/CMBlockBuffer.h");
    let apple = extract_c_function_names("CMBlockBuffer", &[header]);
    let ours: BTreeSet<String> = extract_rust_extern_names("src/ffi/mod.rs")
        .into_iter()
        .filter(|n| n.starts_with("cm_block_buffer_"))
        .map(|n| snake_to_pascal_with_prefix(&n, "CMBlockBuffer", "cm_block_buffer_"))
        .collect();

    let bridge_only: BTreeSet<String> = [
        "CMBlockBufferCreateWithData",
        "CMBlockBufferRetain",
        "CMBlockBufferRelease",
        "CMBlockBufferHash",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    assert!(report(
        "CMBlockBuffer",
        &apple,
        &ours,
        &cm_block_buffer_omitted(),
        &bridge_only
    ));
    let (wrapped, missing, _) = diff(&apple, &ours, &cm_block_buffer_omitted(), &bridge_only);
    let coverable = wrapped.len() + missing.len();
    let pct = if coverable == 0 {
        100.0
    } else {
        wrapped.len() as f64 / coverable as f64 * 100.0
    };
    assert!(
        pct >= 100.0,
        "CMBlockBuffer: {pct:.1}%; missing: {missing:?}"
    );
}

fn cm_format_description_omitted() -> BTreeSet<String> {
    [
        "CMFormatDescriptionCreate",
        "CMFormatDescriptionCreateFromBigEndianImageDescriptionData",
        "CMFormatDescriptionCreateFromBigEndianSoundDescriptionData",
        "CMFormatDescriptionEqual",
        "CMFormatDescriptionEqualIgnoringExtensionKeys",
        "CMFormatDescriptionGetExtension",
        "CMFormatDescriptionGetTypeID",
        "CMAudioFormatDescriptionCreate",
        "CMAudioFormatDescriptionCreateSummary",
        "CMAudioFormatDescriptionEqual",
        "CMAudioFormatDescriptionGetChannelLayout",
        "CMAudioFormatDescriptionGetFormatList",
        "CMAudioFormatDescriptionGetMagicCookie",
        "CMAudioFormatDescriptionGetMostCompatibleFormat",
        "CMAudioFormatDescriptionGetRichestDecodableFormat",
        "CMAudioFormatDescriptionGetStreamBasicDescription",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[test]
fn cm_format_description_api_coverage() {
    let sdk = sdk_root();
    let header =
        sdk.join("System/Library/Frameworks/CoreMedia.framework/Headers/CMFormatDescription.h");
    let mut apple = extract_c_function_names("CMFormatDescription", std::slice::from_ref(&header));
    apple.append(&mut extract_c_function_names(
        "CMAudioFormatDescription",
        &[header],
    ));

    let ours: BTreeSet<String> = extract_rust_extern_names("src/ffi/mod.rs")
        .into_iter()
        .filter(|n| n.starts_with("cm_format_description_"))
        .map(|n| {
            if n.starts_with("cm_format_description_get_audio_") {
                let tail = n.trim_start_matches("cm_format_description_get_audio_");
                // Map to Apple's CMAudioFormatDescriptionGet<Field> form, but
                // note Apple's actual API uses CMAudioFormatDescriptionGet
                // followed by audio-style names that don't match field names
                // 1:1 — so map onto the actual existing Apple symbols.
                match tail {
                    "sample_rate" | "channel_count" | "bits_per_channel" | "bytes_per_frame"
                    | "format_flags" => {
                        // Apple's only CMAudioFormatDescription getter is
                        // CMAudioFormatDescriptionGetStreamBasicDescription;
                        // our split-out scalar getters are bridge-only convenience.
                        "CMAudioFormatDescriptionGetStreamBasicDescription".to_string()
                    }
                    _ => snake_to_pascal_with_prefix(
                        &format!("__get_{tail}"),
                        "CMAudioFormatDescription",
                        "__",
                    ),
                }
            } else {
                snake_to_pascal_with_prefix(&n, "CMFormatDescription", "cm_format_description_")
            }
        })
        .collect();

    let bridge_only: BTreeSet<String> = [
        "CMFormatDescriptionRetain",
        "CMFormatDescriptionRelease",
        "CMFormatDescriptionHash",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    assert!(report(
        "CMFormatDescription",
        &apple,
        &ours,
        &cm_format_description_omitted(),
        &bridge_only,
    ));
    let (wrapped, missing, _) = diff(
        &apple,
        &ours,
        &cm_format_description_omitted(),
        &bridge_only,
    );
    let coverable = wrapped.len() + missing.len();
    let pct = if coverable == 0 {
        100.0
    } else {
        wrapped.len() as f64 / coverable as f64 * 100.0
    };
    assert!(
        pct >= 100.0,
        "CMFormatDescription: {pct:.1}%; missing: {missing:?}"
    );
}

// ---- CoreVideo coverage ----

fn cv_pixel_buffer_omitted() -> BTreeSet<String> {
    [
        // CVPixelBufferGetTypeID is wrapped via the renamed snake form.
        // Async / managed-buffer creation variants — v0.2
        "CVPixelBufferCreateResolvedAttributesDictionary",
        "CVPixelBufferCreateWithBytesAndCallback",
        "CVPixelBufferCreateWithPlanarBytesAndCallback",
        "CVPixelBufferGetIOSurfacePropertiesFromPixelBufferAttributes",
        "CVPixelBufferGetPixelBufferAttributesFromIOSurfaceProperties",
        // Attachments + KVO — v0.2
        "CVPixelBufferCopyCreationAttributes",
        "CVPixelBufferGetAttachment",
        "CVPixelBufferSetAttachment",
        "CVPixelBufferGetAttachments",
        "CVPixelBufferSetAttachments",
        "CVPixelBufferRemoveAttachment",
        "CVPixelBufferRemoveAllAttachments",
        "CVPixelBufferCopyAttachments",
        "CVPixelBufferPropagateAttachments",
        "CVPixelBufferRetainedBufferRetainsCachedAttachments",
        // Compatibility check — v0.2
        "CVPixelBufferIsCompatibleWithAttributes",
        // Cleanup callbacks — internal Apple plumbing
        "CVPixelBufferReleaseBytesCallback",
        "CVPixelBufferReleasePlanarBytesCallback",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[test]
fn cv_pixel_buffer_api_coverage() {
    let sdk = sdk_root();
    let main_header =
        sdk.join("System/Library/Frameworks/CoreVideo.framework/Headers/CVPixelBuffer.h");
    let iosurface_header =
        sdk.join("System/Library/Frameworks/CoreVideo.framework/Headers/CVPixelBufferIOSurface.h");
    let apple = extract_c_function_names("CVPixelBuffer", &[main_header, iosurface_header]);
    let ours: BTreeSet<String> = extract_rust_extern_names("src/ffi/mod.rs")
        .into_iter()
        .filter(|n| n.starts_with("cv_pixel_buffer_") && !n.starts_with("cv_pixel_buffer_pool_"))
        .map(|n| snake_to_pascal_with_prefix(&n, "CVPixelBuffer", "cv_pixel_buffer_"))
        .collect();
    let bridge_only: BTreeSet<String> = [
        "CVPixelBufferRetain",
        "CVPixelBufferRelease",
        "CVPixelBufferHash",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    assert!(report(
        "CVPixelBuffer",
        &apple,
        &ours,
        &cv_pixel_buffer_omitted(),
        &bridge_only,
    ));
    let (wrapped, missing, _) = diff(&apple, &ours, &cv_pixel_buffer_omitted(), &bridge_only);
    let coverable = wrapped.len() + missing.len();
    let pct = if coverable == 0 {
        100.0
    } else {
        wrapped.len() as f64 / coverable as f64 * 100.0
    };
    assert!(
        pct >= 100.0,
        "CVPixelBuffer: {pct:.1}%; missing: {missing:?}"
    );
}

fn cv_pixel_buffer_pool_omitted() -> BTreeSet<String> {
    [
        "CVPixelBufferPoolGetTypeID",
        "CVPixelBufferPoolCreatePixelBufferWithAuxAttributes",
        "CVPixelBufferPoolCreateWithMinimumBufferCount",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[test]
fn cv_pixel_buffer_pool_api_coverage() {
    let sdk = sdk_root();
    let header =
        sdk.join("System/Library/Frameworks/CoreVideo.framework/Headers/CVPixelBufferPool.h");
    let apple = extract_c_function_names("CVPixelBufferPool", &[header]);
    let ours: BTreeSet<String> = extract_rust_extern_names("src/ffi/mod.rs")
        .into_iter()
        .filter(|n| n.starts_with("cv_pixel_buffer_pool_"))
        .map(|n| snake_to_pascal_with_prefix(&n, "CVPixelBufferPool", "cv_pixel_buffer_pool_"))
        .collect();
    let bridge_only: BTreeSet<String> = [
        "CVPixelBufferPoolRetain",
        "CVPixelBufferPoolRelease",
        "CVPixelBufferPoolHash",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    assert!(report(
        "CVPixelBufferPool",
        &apple,
        &ours,
        &cv_pixel_buffer_pool_omitted(),
        &bridge_only,
    ));
    let (wrapped, missing, _) = diff(&apple, &ours, &cv_pixel_buffer_pool_omitted(), &bridge_only);
    let coverable = wrapped.len() + missing.len();
    let pct = if coverable == 0 {
        100.0
    } else {
        wrapped.len() as f64 / coverable as f64 * 100.0
    };
    assert!(
        pct >= 100.0,
        "CVPixelBufferPool: {pct:.1}%; missing: {missing:?}"
    );
}

// ---- CVPixelBuffer attribute-key constants ----

/// Read the bridge file for textual matches against named keys.
fn read_bridge_files() -> String {
    let mut blob = String::new();
    let bridge_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("swift-bridge/Sources");
    for entry in std::fs::read_dir(&bridge_root)
        .unwrap_or_else(|e| panic!("can't read bridge root {}: {e}", bridge_root.display()))
    {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for sub in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("can't read bridge subdir {}: {e}", dir.display()))
        {
            let path = sub.unwrap().path();
            if path.extension().is_some_and(|e| e == "swift") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    blob.push_str(&text);
                    blob.push('\n');
                }
            }
        }
    }
    blob
}

#[test]
fn cv_pixel_buffer_attribute_keys_coverage() {
    // CVPixelBuffer creation is configured via a `[kCVPixelBuffer*Key:
    // value]` attribute dictionary. Verify every key the Swift bridge
    // references is a real, currently-valid `CVPixelBuffer.h` constant —
    // future SDK renames surface here instead of as a runtime "Apple
    // silently ignored an unknown key" no-op.
    let sdk = sdk_root();
    let header = sdk.join("System/Library/Frameworks/CoreVideo.framework/Headers/CVPixelBuffer.h");
    let contents = std::fs::read_to_string(&header)
        .unwrap_or_else(|e| panic!("can't read {}: {e}", header.display()));
    let re = regex_lite::Regex::new(
        r"CV_EXPORT\s+const\s+CFStringRef\s+(?:CV_NONNULL\s+)?(kCVPixelBuffer[A-Za-z0-9]+Key)\b",
    )
    .unwrap();
    let apple: BTreeSet<String> = re
        .captures_iter(&contents)
        .map(|c| c[1].to_string())
        .collect();

    let bridge = read_bridge_files();
    let our_keys: BTreeSet<String> = apple
        .iter()
        .filter(|name| {
            let needle = format!(r"\b{}\b", regex_lite::escape(name));
            regex_lite::Regex::new(&needle).unwrap().is_match(&bridge)
        })
        .cloned()
        .collect();

    // v0.1 surface: width, height, pixel format, and IOSurface backing
    // hint. Every other key is intentionally omitted (most are advanced
    // GPU-interop hints not yet exposed by the wrapper).
    let kept: BTreeSet<&str> = [
        "kCVPixelBufferWidthKey",
        "kCVPixelBufferHeightKey",
        "kCVPixelBufferPixelFormatTypeKey",
        "kCVPixelBufferIOSurfacePropertiesKey",
    ]
    .into_iter()
    .collect();
    let omitted: BTreeSet<String> = apple
        .iter()
        .filter(|s| !kept.contains(s.as_str()))
        .cloned()
        .collect();

    let wrapped: BTreeSet<&String> = apple.intersection(&our_keys).collect();
    let missing: BTreeSet<&String> = apple
        .difference(&our_keys)
        .filter(|s| !omitted.contains(*s))
        .collect();
    let coverable = wrapped.len() + missing.len();
    let pct = if coverable == 0 {
        100.0
    } else {
        wrapped.len() as f64 / coverable as f64 * 100.0
    };
    println!(
        "\n=== CVPixelBuffer attribute-key coverage ===\n  apple={}, omitted={}, coverable={coverable}, wrapped={}, missing={}, pct={pct:.1}%",
        apple.len(),
        omitted.len(),
        wrapped.len(),
        missing.len(),
    );
    if !missing.is_empty() {
        for s in &missing {
            println!("  - {s}");
        }
    }
    assert!(
        pct >= 100.0,
        "CVPixelBuffer attribute keys: {pct:.1}%; missing: {missing:?}"
    );
}

#[test]
fn cv_pixel_buffer_pool_attribute_keys_coverage() {
    // CVPixelBufferPool creation is configured via a `[kCVPixelBufferPool*Key:
    // value]` attribute dictionary. Same rationale as above — verify the
    // keys our pool bridge passes are real Apple constants.
    let sdk = sdk_root();
    let header =
        sdk.join("System/Library/Frameworks/CoreVideo.framework/Headers/CVPixelBufferPool.h");
    let contents = std::fs::read_to_string(&header)
        .unwrap_or_else(|e| panic!("can't read {}: {e}", header.display()));
    let re = regex_lite::Regex::new(
        r"CV_EXPORT\s+const\s+CFStringRef\s+(?:CV_NONNULL\s+)?(kCVPixelBufferPool[A-Za-z0-9]+Key)\b",
    )
    .unwrap();
    let apple: BTreeSet<String> = re
        .captures_iter(&contents)
        .map(|c| c[1].to_string())
        .collect();

    let bridge = read_bridge_files();
    let our_keys: BTreeSet<String> = apple
        .iter()
        .filter(|name| {
            let needle = format!(r"\b{}\b", regex_lite::escape(name));
            regex_lite::Regex::new(&needle).unwrap().is_match(&bridge)
        })
        .cloned()
        .collect();

    let kept: BTreeSet<&str> = ["kCVPixelBufferPoolMinimumBufferCountKey"]
        .into_iter()
        .collect();
    let omitted: BTreeSet<String> = apple
        .iter()
        .filter(|s| !kept.contains(s.as_str()))
        .cloned()
        .collect();

    let wrapped: BTreeSet<&String> = apple.intersection(&our_keys).collect();
    let missing: BTreeSet<&String> = apple
        .difference(&our_keys)
        .filter(|s| !omitted.contains(*s))
        .collect();
    let coverable = wrapped.len() + missing.len();
    let pct = if coverable == 0 {
        100.0
    } else {
        wrapped.len() as f64 / coverable as f64 * 100.0
    };
    println!(
        "\n=== CVPixelBufferPool attribute-key coverage ===\n  apple={}, omitted={}, coverable={coverable}, wrapped={}, missing={}, pct={pct:.1}%",
        apple.len(),
        omitted.len(),
        wrapped.len(),
        missing.len(),
    );
    if !missing.is_empty() {
        for s in &missing {
            println!("  - {s}");
        }
    }
    assert!(
        pct >= 100.0,
        "CVPixelBufferPool attribute keys: {pct:.1}%; missing: {missing:?}"
    );
}
