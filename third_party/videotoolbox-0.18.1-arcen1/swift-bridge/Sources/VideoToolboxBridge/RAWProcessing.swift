// VTRAWProcessingSession bridge — only the async-block bits that
// can't be called cleanly from Rust's C FFI live in Swift. All the
// Create/Invalidate/CopyParameters/SetParameters/CompleteFrames C
// entry points are called directly from Rust via the pure-C FFI in
// `src/ffi/mod.rs` (the Swift overlay hides them with
// `CF_REFINED_FOR_SWIFT`, but they remain linkable from C/Rust).

import CoreFoundation
import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

public typealias VTBRawParameterChangedCallback = @convention(c) (
    UnsafeMutableRawPointer?,
    CFArray?
) -> Void
public typealias VTBRawProcessFrameAsyncCallback = @convention(c) (
    UnsafeMutableRawPointer?,
    Int32,
    UnsafeMutableRawPointer?
) -> Void

typealias VTBRawParameterChangedHandler = @convention(block) (CFArray?) -> Void

@available(macOS 26.0, *)
@_silgen_name("VTRAWProcessingSessionSetParameterChangedHandler")
private func vtb_raw_processing_session_set_parameter_changed_handler(
    _ session: VTRAWProcessingSession,
    _ parameterChangedHandler: VTBRawParameterChangedHandler?
) -> OSStatus

/// Process a single RAW frame synchronously, returning the
/// processed `CVPixelBuffer` (retained +1) via `out`.
@_cdecl("vtb_raw_session_process_frame")
public func vtb_raw_session_process_frame(
    _ session: UnsafeMutableRawPointer,
    _ inputPixelBuffer: UnsafeMutableRawPointer,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 15.0, *) {
        let s: VTRAWProcessingSession = vtb_borrow(session)
        let pb: CVPixelBuffer = vtb_borrow(inputPixelBuffer)
        return vtb_block_on_async(
            work: { try await s.process(frame: pb) },
            onSuccess: { processed in
                out.pointee = vtb_retain(processed)
            }
        )
    }
    return VTB_NOT_SUPPORTED
}

/// Process a single RAW frame asynchronously, reporting the retained output
/// pixel buffer through a C callback.
@_cdecl("vtb_raw_session_process_frame_async")
public func vtb_raw_session_process_frame_async(
    _ session: UnsafeMutableRawPointer,
    _ inputPixelBuffer: UnsafeMutableRawPointer,
    _ refcon: UnsafeMutableRawPointer?,
    _ callback: VTBRawProcessFrameAsyncCallback?
) -> Int32 {
    guard let callback else {
        return -50
    }
    if #available(macOS 15.0, *) {
        let s: VTRAWProcessingSession = vtb_borrow(session)
        let pb: CVPixelBuffer = vtb_borrow(inputPixelBuffer)
        Task {
            do {
                let processed = try await s.process(frame: pb)
                callback(refcon, 0, vtb_retain(processed))
            } catch {
                callback(refcon, vtb_status(from: error), nil)
            }
        }
        return 0
    }
    return VTB_NOT_SUPPORTED
}

/// Install or clear the RAW-parameter change handler.
@_cdecl("vtb_raw_session_set_parameter_changed_handler")
public func vtb_raw_session_set_parameter_changed_handler(
    _ session: UnsafeMutableRawPointer,
    _ refcon: UnsafeMutableRawPointer?,
    _ callback: VTBRawParameterChangedCallback?
) -> Int32 {
    if #available(macOS 26.0, *) {
        let s: VTRAWProcessingSession = vtb_borrow(session)
        let handler = callback.map { callback in
            { (newParameters: CFArray?) in
                callback(refcon, newParameters)
            }
        }
        return vtb_raw_processing_session_set_parameter_changed_handler(s, handler)
    }
    return VTB_NOT_SUPPORTED
}
