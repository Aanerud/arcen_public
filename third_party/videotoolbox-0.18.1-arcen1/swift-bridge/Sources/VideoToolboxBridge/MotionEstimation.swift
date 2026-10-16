// VTMotionEstimationSession bridge.
//
// Wraps Apple's macOS 26+ motion-estimation API in a Swift surface
// callable from Rust as plain `@_cdecl` C functions. The session is
// fed two `CVReadOnlyPixelBuffer`s and returns a `CVPixelBuffer` of
// motion vectors via the async `motion(of:comparedTo:)` API.

import CoreFoundation
import CoreVideo
import Foundation
import VideoToolbox

private typealias VTBMotionEstimationOutputHandler = @convention(block) (
    OSStatus,
    UInt32,
    CFDictionary?,
    CVPixelBuffer?
) -> Void

@available(macOS 26.0, *)
@_silgen_name("VTMotionEstimationSessionEstimateMotionVectors")
private func vtb_motion_estimation_session_estimate_motion_vectors(
    _ session: VTMotionEstimationSession,
    _ referenceImage: CVPixelBuffer,
    _ currentImage: CVPixelBuffer,
    _ frameFlags: UInt32,
    _ additionalFrameOptions: CFDictionary?,
    _ outputHandler: @escaping VTBMotionEstimationOutputHandler
) -> OSStatus

// MARK: - Lifecycle

@_cdecl("vtb_motion_session_create")
public func vtb_motion_session_create(
    _ width: UInt32,
    _ height: UInt32,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    if #available(macOS 26.0, *) {
        do {
            let s = try VTMotionEstimationSession(width: width, height: height)
            out.pointee = vtb_retain(s)
            return 0
        } catch {
            out.pointee = nil
            return vtb_status(from: error)
        }
    }
    out.pointee = nil
    return VTB_NOT_SUPPORTED
}

@_cdecl("vtb_motion_session_release")
public func vtb_motion_session_release(_ session: UnsafeMutableRawPointer) {
    if #available(macOS 26.0, *) {
        vtb_release(session, as: VTMotionEstimationSession.self)
    }
}

// MARK: - Estimation

/// Estimate motion vectors between two pixel buffers. Returns the
/// motion-vector `CVPixelBuffer` (retained +1) via `out`, plus any
/// `VTMotionEstimationInfoFlags` in `infoFlagsOut`.
@_cdecl("vtb_motion_session_estimate")
public func vtb_motion_session_estimate(
    _ session: UnsafeMutableRawPointer,
    _ referenceImage: UnsafeMutableRawPointer,
    _ currentImage: UnsafeMutableRawPointer,
    _ frameFlags: UInt32,
    _ infoFlagsOut: UnsafeMutablePointer<UInt32>?,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    infoFlagsOut?.pointee = 0
    if #available(macOS 26.0, *) {
        let s: VTMotionEstimationSession = vtb_borrow(session)
        let refImg: CVPixelBuffer = vtb_borrow(referenceImage)
        let curImg: CVPixelBuffer = vtb_borrow(currentImage)
        let sem = DispatchSemaphore(value: 0)
        var completionStatus: Int32 = 0
        let submitStatus = vtb_motion_estimation_session_estimate_motion_vectors(
            s,
            refImg,
            curImg,
            frameFlags,
            nil
        ) { status, infoFlags, _, motionVectors in
            completionStatus = status
            infoFlagsOut?.pointee = infoFlags
            if let motionVectors {
                out.pointee = vtb_retain(motionVectors)
            }
            sem.signal()
        }
        if submitStatus != 0 {
            return submitStatus
        }
        sem.wait()
        return completionStatus
    }
    return VTB_NOT_SUPPORTED
}
