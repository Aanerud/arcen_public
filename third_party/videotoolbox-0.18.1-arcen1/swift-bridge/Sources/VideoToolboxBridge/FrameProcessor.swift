// VTFrameProcessor bridge — capability queries + retained wrappers for
// frames / optical flow + full per-filter processing entry points.

import CoreMedia
import CoreVideo
import Foundation
import Metal
import VideoToolbox

public let VTB_PARAM_ERR: Int32 = -50

@available(macOS 15.4, *)
private final class VTBFrameProcessorCommandBufferTicket {
    let processor: VTFrameProcessor
    let parameters: AnyObject

    init(processor: VTFrameProcessor, parameters: AnyObject) {
        self.processor = processor
        self.parameters = parameters
    }
}

@inline(__always)
private func vtb_borrow_optional<T: AnyObject>(_ ptr: UnsafeMutableRawPointer?) -> T? {
    guard let ptr else { return nil }
    return vtb_borrow(ptr)
}

@inline(__always)
private func vtb_borrow_command_buffer(
    _ ptr: UnsafeMutableRawPointer?
) -> (any MTLCommandBuffer)? {
    guard let ptr else { return nil }
    return Unmanaged<AnyObject>.fromOpaque(ptr).takeUnretainedValue() as? any MTLCommandBuffer
}

@available(macOS 15.4, *)
private func vtb_borrow_frames(
    _ ptrs: UnsafePointer<UnsafeMutableRawPointer?>?,
    count: Int
) -> [VTFrameProcessorFrame]? {
    guard count >= 0 else { return nil }
    guard count > 0 else { return [] }
    guard let ptrs else { return nil }

    var frames = [VTFrameProcessorFrame]()
    frames.reserveCapacity(count)
    for idx in 0..<count {
        guard let frame: VTFrameProcessorFrame = vtb_borrow_optional(ptrs[idx]) else {
            return nil
        }
        frames.append(frame)
    }
    return frames
}

private func vtb_numbers(
    _ values: UnsafePointer<Float>?,
    count: Int
) -> [Float]? {
    guard count >= 0 else { return nil }
    guard count > 0 else { return [] }
    guard let values else { return nil }
    return (0..<count).map { values[$0] }
}

@available(macOS 15.4, *)
private func vtb_process_completion(
    _ processor: VTFrameProcessor,
    parameters: any VTFrameProcessorParameters
) -> Int32 {
    let sem = DispatchSemaphore(value: 0)
    var status: Int32 = 0
    processor.process(parameters: parameters) { _, error in
        if let error {
            status = vtb_status(from: error)
        }
        sem.signal()
    }
    sem.wait()
    return status
}

@available(macOS 15.4, *)
private func vtb_process_command_buffer(
    _ processor: VTFrameProcessor,
    commandBuffer: any MTLCommandBuffer,
    parameters: AnyObject & VTFrameProcessorParameters
) -> Int32 {
    let ticket = VTBFrameProcessorCommandBufferTicket(processor: processor, parameters: parameters)
    commandBuffer.addCompletedHandler { _ in
        _ = ticket
    }
    processor.process(with: commandBuffer, parameters: parameters)
    return 0
}

@available(macOS 26.0, *)
private func vtb_super_resolution_configuration(
    frameWidth: Int,
    frameHeight: Int,
    scaleFactor: Int,
    usePrecomputedFlow: Bool,
    inputTypeRaw: Int,
    qualityPrioritizationRaw: Int,
    revisionRaw: Int
) -> VTSuperResolutionScalerConfiguration? {
    guard let inputType = VTSuperResolutionScalerConfiguration.InputType(rawValue: inputTypeRaw),
          let qualityPrioritization = VTSuperResolutionScalerConfiguration.QualityPrioritization(rawValue: qualityPrioritizationRaw),
          let revision = VTSuperResolutionScalerConfiguration.Revision(rawValue: revisionRaw) else {
        return nil
    }
    return VTSuperResolutionScalerConfiguration(
        frameWidth: frameWidth,
        frameHeight: frameHeight,
        scaleFactor: scaleFactor,
        inputType: inputType,
        usePrecomputedFlow: usePrecomputedFlow,
        qualityPrioritization: qualityPrioritization,
        revision: revision
    )
}

// MARK: - Capability queries

@_cdecl("vt_super_resolution_is_supported")
public func vt_super_resolution_is_supported() -> Bool {
    if #available(macOS 26.0, *) { return VTSuperResolutionScalerConfiguration.isSupported }
    return false
}

@_cdecl("vt_motion_blur_is_supported")
public func vt_motion_blur_is_supported() -> Bool {
    if #available(macOS 15.4, *) { return VTMotionBlurConfiguration.isSupported }
    return false
}

@_cdecl("vt_temporal_noise_filter_is_supported")
public func vt_temporal_noise_filter_is_supported() -> Bool {
    if #available(macOS 26.0, *) { return VTTemporalNoiseFilterConfiguration.isSupported }
    return false
}

@_cdecl("vt_frame_rate_conversion_is_supported")
public func vt_frame_rate_conversion_is_supported() -> Bool {
    if #available(macOS 15.4, *) { return VTFrameRateConversionConfiguration.isSupported }
    return false
}

@_cdecl("vt_low_latency_super_resolution_is_supported")
public func vt_low_latency_super_resolution_is_supported() -> Bool {
    if #available(macOS 26.0, *) { return VTLowLatencySuperResolutionScalerConfiguration.isSupported }
    return false
}

@_cdecl("vt_low_latency_frame_interpolation_is_supported")
public func vt_low_latency_frame_interpolation_is_supported() -> Bool {
    if #available(macOS 26.0, *) { return VTLowLatencyFrameInterpolationConfiguration.isSupported }
    return false
}

@_cdecl("vt_optical_flow_is_supported")
public func vt_optical_flow_is_supported() -> Bool {
    if #available(macOS 15.4, *) { return VTOpticalFlowConfiguration.isSupported }
    return false
}

@_cdecl("vt_super_resolution_supported_scale_factors")
public func vt_super_resolution_supported_scale_factors(
    _ outBuf: UnsafeMutablePointer<UInt32>,
    _ max: Int
) -> Int {
    if #available(macOS 26.0, *) {
        let arr = VTSuperResolutionScalerConfiguration.supportedScaleFactors
        let n = min(arr.count, max)
        for i in 0..<n {
            outBuf[i] = UInt32(truncatingIfNeeded: arr[i])
        }
        return n
    }
    return 0
}

@_cdecl("vt_low_latency_super_resolution_supported_scale_factors")
public func vt_low_latency_super_resolution_supported_scale_factors(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ outBuf: UnsafeMutablePointer<Float>,
    _ max: Int
) -> Int {
    if #available(macOS 26.0, *) {
        let arr = VTLowLatencySuperResolutionScalerConfiguration.supportedScaleFactors(
            frameWidth: frameWidth,
            frameHeight: frameHeight
        )
        let n = min(arr.count, max)
        for i in 0..<n {
            outBuf[i] = arr[i]
        }
        return n
    }
    return 0
}

@_cdecl("vt_super_resolution_model_status")
public func vt_super_resolution_model_status(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ scaleFactor: Int,
    _ usePrecomputedFlow: Bool,
    _ inputType: Int,
    _ qualityPrioritization: Int,
    _ revision: Int
) -> Int32 {
    if #available(macOS 26.0, *) {
        guard let cfg = vtb_super_resolution_configuration(
            frameWidth: frameWidth,
            frameHeight: frameHeight,
            scaleFactor: scaleFactor,
            usePrecomputedFlow: usePrecomputedFlow,
            inputTypeRaw: inputType,
            qualityPrioritizationRaw: qualityPrioritization,
            revisionRaw: revision
        ) else { return VTB_PARAM_ERR }
        return Int32(cfg.configurationModelStatus.rawValue)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_super_resolution_model_percentage_available")
public func vt_super_resolution_model_percentage_available(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ scaleFactor: Int,
    _ usePrecomputedFlow: Bool,
    _ inputType: Int,
    _ qualityPrioritization: Int,
    _ revision: Int
) -> Float {
    if #available(macOS 26.0, *) {
        guard let cfg = vtb_super_resolution_configuration(
            frameWidth: frameWidth,
            frameHeight: frameHeight,
            scaleFactor: scaleFactor,
            usePrecomputedFlow: usePrecomputedFlow,
            inputTypeRaw: inputType,
            qualityPrioritizationRaw: qualityPrioritization,
            revisionRaw: revision
        ) else { return -1.0 }
        return cfg.configurationModelPercentageAvailable
    }
    return -1.0
}

@_cdecl("vt_super_resolution_download_model")
public func vt_super_resolution_download_model(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ scaleFactor: Int,
    _ usePrecomputedFlow: Bool,
    _ inputType: Int,
    _ qualityPrioritization: Int,
    _ revision: Int
) -> Int32 {
    if #available(macOS 26.0, *) {
        guard let cfg = vtb_super_resolution_configuration(
            frameWidth: frameWidth,
            frameHeight: frameHeight,
            scaleFactor: scaleFactor,
            usePrecomputedFlow: usePrecomputedFlow,
            inputTypeRaw: inputType,
            qualityPrioritizationRaw: qualityPrioritization,
            revisionRaw: revision
        ) else { return VTB_PARAM_ERR }
        let sem = DispatchSemaphore(value: 0)
        var status: Int32 = 0
        cfg.downloadConfigurationModel { error in
            if let error {
                status = vtb_status(from: error)
            }
            sem.signal()
        }
        sem.wait()
        return status
    }
    return VTB_NOT_SUPPORTED
}

// MARK: - Session start helpers

@_cdecl("vt_super_resolution_start")
public func vt_super_resolution_start(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ scaleFactor: Int,
    _ usePrecomputedFlow: Bool,
    _ inputType: Int,
    _ qualityPrioritization: Int,
    _ revision: Int,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 26.0, *) {
        guard let cfg = vtb_super_resolution_configuration(
            frameWidth: frameWidth,
            frameHeight: frameHeight,
            scaleFactor: scaleFactor,
            usePrecomputedFlow: usePrecomputedFlow,
            inputTypeRaw: inputType,
            qualityPrioritizationRaw: qualityPrioritization,
            revisionRaw: revision
        ) else { return VTB_PARAM_ERR }
        let p = VTFrameProcessor()
        do {
            try p.startSession(configuration: cfg)
            out.pointee = vtb_retain(p)
            return 0
        } catch {
            return vtb_status(from: error)
        }
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_motion_blur_start")
public func vt_motion_blur_start(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ usePrecomputedFlow: Bool,
    _ qualityPrioritization: Int,
    _ revision: Int,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 15.4, *) {
        guard let qualityPrioritization = VTMotionBlurConfiguration.QualityPrioritization(rawValue: qualityPrioritization),
              let revision = VTMotionBlurConfiguration.Revision(rawValue: revision),
              let cfg = VTMotionBlurConfiguration(
                frameWidth: frameWidth,
                frameHeight: frameHeight,
                usePrecomputedFlow: usePrecomputedFlow,
                qualityPrioritization: qualityPrioritization,
                revision: revision
              ) else { return VTB_PARAM_ERR }
        let p = VTFrameProcessor()
        do {
            try p.startSession(configuration: cfg)
            out.pointee = vtb_retain(p)
            return 0
        } catch {
            return vtb_status(from: error)
        }
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_temporal_noise_filter_start")
public func vt_temporal_noise_filter_start(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ sourcePixelFormat: UInt32,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 26.0, *) {
        guard let cfg = VTTemporalNoiseFilterConfiguration(
            frameWidth: frameWidth,
            frameHeight: frameHeight,
            sourcePixelFormat: sourcePixelFormat
        ) else { return VTB_NOT_SUPPORTED }
        let p = VTFrameProcessor()
        do {
            try p.startSession(configuration: cfg)
            out.pointee = vtb_retain(p)
            return 0
        } catch {
            return vtb_status(from: error)
        }
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_rate_conversion_start")
public func vt_frame_rate_conversion_start(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ usePrecomputedFlow: Bool,
    _ qualityPrioritization: Int,
    _ revision: Int,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 15.4, *) {
        guard let qualityPrioritization = VTFrameRateConversionConfiguration.QualityPrioritization(rawValue: qualityPrioritization),
              let revision = VTFrameRateConversionConfiguration.Revision(rawValue: revision),
              let cfg = VTFrameRateConversionConfiguration(
                frameWidth: frameWidth,
                frameHeight: frameHeight,
                usePrecomputedFlow: usePrecomputedFlow,
                qualityPrioritization: qualityPrioritization,
                revision: revision
              ) else { return VTB_PARAM_ERR }
        let p = VTFrameProcessor()
        do {
            try p.startSession(configuration: cfg)
            out.pointee = vtb_retain(p)
            return 0
        } catch {
            return vtb_status(from: error)
        }
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_low_latency_super_resolution_start")
public func vt_low_latency_super_resolution_start(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ scaleFactor: Float,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 26.0, *) {
        let cfg = VTLowLatencySuperResolutionScalerConfiguration(
            frameWidth: frameWidth,
            frameHeight: frameHeight,
            scaleFactor: scaleFactor
        )
        let p = VTFrameProcessor()
        do {
            try p.startSession(configuration: cfg)
            out.pointee = vtb_retain(p)
            return 0
        } catch {
            return vtb_status(from: error)
        }
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_low_latency_frame_interpolation_start")
public func vt_low_latency_frame_interpolation_start(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ numberOfInterpolatedFrames: Int,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 26.0, *) {
        guard let cfg = VTLowLatencyFrameInterpolationConfiguration(
            frameWidth: frameWidth,
            frameHeight: frameHeight,
            numberOfInterpolatedFrames: numberOfInterpolatedFrames
        ) else { return VTB_NOT_SUPPORTED }
        let p = VTFrameProcessor()
        do {
            try p.startSession(configuration: cfg)
            out.pointee = vtb_retain(p)
            return 0
        } catch {
            return vtb_status(from: error)
        }
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_optical_flow_start")
public func vt_optical_flow_start(
    _ frameWidth: Int,
    _ frameHeight: Int,
    _ qualityPrioritization: Int,
    _ revision: Int,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 15.4, *) {
        guard let qualityPrioritization = VTOpticalFlowConfiguration.QualityPrioritization(rawValue: qualityPrioritization),
              let revision = VTOpticalFlowConfiguration.Revision(rawValue: revision),
              let cfg = VTOpticalFlowConfiguration(
                frameWidth: frameWidth,
                frameHeight: frameHeight,
                qualityPrioritization: qualityPrioritization,
                revision: revision
              ) else { return VTB_PARAM_ERR }
        let p = VTFrameProcessor()
        do {
            try p.startSession(configuration: cfg)
            out.pointee = vtb_retain(p)
            return 0
        } catch {
            return vtb_status(from: error)
        }
    }
    return VTB_NOT_SUPPORTED
}

// MARK: - Frame / flow wrappers

@_cdecl("vt_frame_processor_frame_create")
public func vt_frame_processor_frame_create(
    _ buffer: UnsafeMutableRawPointer,
    _ presentationTimeStamp: CMTime,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 15.4, *) {
        let pb: CVPixelBuffer = vtb_borrow(buffer)
        guard let frame = VTFrameProcessorFrame(
            buffer: pb,
            presentationTimeStamp: presentationTimeStamp
        ) else { return VTB_PARAM_ERR }
        out.pointee = vtb_retain(frame)
        return 0
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_frame_release")
public func vt_frame_processor_frame_release(_ frame: UnsafeMutableRawPointer) {
    if #available(macOS 15.4, *) {
        vtb_release(frame, as: VTFrameProcessorFrame.self)
    }
}

@_cdecl("vt_frame_processor_optical_flow_create")
public func vt_frame_processor_optical_flow_create(
    _ forwardFlow: UnsafeMutableRawPointer,
    _ backwardFlow: UnsafeMutableRawPointer,
    _ out: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    out.pointee = nil
    if #available(macOS 15.4, *) {
        let forward: CVPixelBuffer = vtb_borrow(forwardFlow)
        let backward: CVPixelBuffer = vtb_borrow(backwardFlow)
        guard let flow = VTFrameProcessorOpticalFlow(
            forwardFlow: forward,
            backwardFlow: backward
        ) else { return VTB_PARAM_ERR }
        out.pointee = vtb_retain(flow)
        return 0
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_optical_flow_release")
public func vt_frame_processor_optical_flow_release(_ flow: UnsafeMutableRawPointer) {
    if #available(macOS 15.4, *) {
        vtb_release(flow, as: VTFrameProcessorOpticalFlow.self)
    }
}

// MARK: - Processing helpers

@_cdecl("vt_frame_processor_process_super_resolution")
public func vt_frame_processor_process_super_resolution(
    _ processor: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ previousFrame: UnsafeMutableRawPointer?,
    _ previousOutputFrame: UnsafeMutableRawPointer?,
    _ opticalFlow: UnsafeMutableRawPointer?,
    _ submissionMode: Int,
    _ destinationFrame: UnsafeMutableRawPointer
) -> Int32 {
    if #available(macOS 26.0, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let destination: VTFrameProcessorFrame = vtb_borrow(destinationFrame)
        let previous: VTFrameProcessorFrame? = vtb_borrow_optional(previousFrame)
        let previousOutput: VTFrameProcessorFrame? = vtb_borrow_optional(previousOutputFrame)
        let flow: VTFrameProcessorOpticalFlow? = vtb_borrow_optional(opticalFlow)
        guard let params = VTSuperResolutionScalerParameters(
            sourceFrame: source,
            previousFrame: previous,
            previousOutputFrame: previousOutput,
            opticalFlow: flow,
            submissionMode: VTSuperResolutionScalerParameters.SubmissionMode(rawValue: submissionMode) ?? .random,
            destinationFrame: destination
        ) else { return VTB_PARAM_ERR }
        return vtb_process_completion(p, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_super_resolution_with_command_buffer")
public func vt_frame_processor_process_super_resolution_with_command_buffer(
    _ processor: UnsafeMutableRawPointer,
    _ commandBuffer: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ previousFrame: UnsafeMutableRawPointer?,
    _ previousOutputFrame: UnsafeMutableRawPointer?,
    _ opticalFlow: UnsafeMutableRawPointer?,
    _ submissionMode: Int,
    _ destinationFrame: UnsafeMutableRawPointer
) -> Int32 {
    if #available(macOS 26.0, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        guard let cb = vtb_borrow_command_buffer(commandBuffer) else { return VTB_PARAM_ERR }
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let destination: VTFrameProcessorFrame = vtb_borrow(destinationFrame)
        let previous: VTFrameProcessorFrame? = vtb_borrow_optional(previousFrame)
        let previousOutput: VTFrameProcessorFrame? = vtb_borrow_optional(previousOutputFrame)
        let flow: VTFrameProcessorOpticalFlow? = vtb_borrow_optional(opticalFlow)
        guard let params = VTSuperResolutionScalerParameters(
            sourceFrame: source,
            previousFrame: previous,
            previousOutputFrame: previousOutput,
            opticalFlow: flow,
            submissionMode: VTSuperResolutionScalerParameters.SubmissionMode(rawValue: submissionMode) ?? .random,
            destinationFrame: destination
        ) else { return VTB_PARAM_ERR }
        return vtb_process_command_buffer(p, commandBuffer: cb, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_motion_blur")
public func vt_frame_processor_process_motion_blur(
    _ processor: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ nextFrame: UnsafeMutableRawPointer?,
    _ previousFrame: UnsafeMutableRawPointer?,
    _ nextOpticalFlow: UnsafeMutableRawPointer?,
    _ previousOpticalFlow: UnsafeMutableRawPointer?,
    _ motionBlurStrength: Int,
    _ submissionMode: Int,
    _ destinationFrame: UnsafeMutableRawPointer
) -> Int32 {
    if #available(macOS 15.4, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let destination: VTFrameProcessorFrame = vtb_borrow(destinationFrame)
        let next: VTFrameProcessorFrame? = vtb_borrow_optional(nextFrame)
        let previous: VTFrameProcessorFrame? = vtb_borrow_optional(previousFrame)
        let nextFlow: VTFrameProcessorOpticalFlow? = vtb_borrow_optional(nextOpticalFlow)
        let previousFlow: VTFrameProcessorOpticalFlow? = vtb_borrow_optional(previousOpticalFlow)
        guard let params = VTMotionBlurParameters(
            sourceFrame: source,
            nextFrame: next,
            previousFrame: previous,
            nextOpticalFlow: nextFlow,
            previousOpticalFlow: previousFlow,
            motionBlurStrength: motionBlurStrength,
            submissionMode: VTMotionBlurParameters.SubmissionMode(rawValue: submissionMode) ?? .random,
            destinationFrame: destination
        ) else { return VTB_PARAM_ERR }
        return vtb_process_completion(p, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_motion_blur_with_command_buffer")
public func vt_frame_processor_process_motion_blur_with_command_buffer(
    _ processor: UnsafeMutableRawPointer,
    _ commandBuffer: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ nextFrame: UnsafeMutableRawPointer?,
    _ previousFrame: UnsafeMutableRawPointer?,
    _ nextOpticalFlow: UnsafeMutableRawPointer?,
    _ previousOpticalFlow: UnsafeMutableRawPointer?,
    _ motionBlurStrength: Int,
    _ submissionMode: Int,
    _ destinationFrame: UnsafeMutableRawPointer
) -> Int32 {
    if #available(macOS 15.4, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        guard let cb = vtb_borrow_command_buffer(commandBuffer) else { return VTB_PARAM_ERR }
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let destination: VTFrameProcessorFrame = vtb_borrow(destinationFrame)
        let next: VTFrameProcessorFrame? = vtb_borrow_optional(nextFrame)
        let previous: VTFrameProcessorFrame? = vtb_borrow_optional(previousFrame)
        let nextFlow: VTFrameProcessorOpticalFlow? = vtb_borrow_optional(nextOpticalFlow)
        let previousFlow: VTFrameProcessorOpticalFlow? = vtb_borrow_optional(previousOpticalFlow)
        guard let params = VTMotionBlurParameters(
            sourceFrame: source,
            nextFrame: next,
            previousFrame: previous,
            nextOpticalFlow: nextFlow,
            previousOpticalFlow: previousFlow,
            motionBlurStrength: motionBlurStrength,
            submissionMode: VTMotionBlurParameters.SubmissionMode(rawValue: submissionMode) ?? .random,
            destinationFrame: destination
        ) else { return VTB_PARAM_ERR }
        return vtb_process_command_buffer(p, commandBuffer: cb, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_temporal_noise_filter")
public func vt_frame_processor_process_temporal_noise_filter(
    _ processor: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ nextFrames: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ nextFrameCount: Int,
    _ previousFrames: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ previousFrameCount: Int,
    _ destinationFrame: UnsafeMutableRawPointer,
    _ filterStrength: Float,
    _ hasDiscontinuity: Bool
) -> Int32 {
    if #available(macOS 26.0, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let destination: VTFrameProcessorFrame = vtb_borrow(destinationFrame)
        guard let next = vtb_borrow_frames(nextFrames, count: nextFrameCount),
              let previous = vtb_borrow_frames(previousFrames, count: previousFrameCount),
              let params = VTTemporalNoiseFilterParameters(
                sourceFrame: source,
                nextFrames: next,
                previousFrames: previous,
                destinationFrame: destination,
                filterStrength: filterStrength,
                hasDiscontinuity: hasDiscontinuity
              ) else { return VTB_PARAM_ERR }
        return vtb_process_completion(p, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_temporal_noise_filter_with_command_buffer")
public func vt_frame_processor_process_temporal_noise_filter_with_command_buffer(
    _ processor: UnsafeMutableRawPointer,
    _ commandBuffer: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ nextFrames: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ nextFrameCount: Int,
    _ previousFrames: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ previousFrameCount: Int,
    _ destinationFrame: UnsafeMutableRawPointer,
    _ filterStrength: Float,
    _ hasDiscontinuity: Bool
) -> Int32 {
    if #available(macOS 26.0, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        guard let cb = vtb_borrow_command_buffer(commandBuffer) else { return VTB_PARAM_ERR }
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let destination: VTFrameProcessorFrame = vtb_borrow(destinationFrame)
        guard let next = vtb_borrow_frames(nextFrames, count: nextFrameCount),
              let previous = vtb_borrow_frames(previousFrames, count: previousFrameCount),
              let params = VTTemporalNoiseFilterParameters(
                sourceFrame: source,
                nextFrames: next,
                previousFrames: previous,
                destinationFrame: destination,
                filterStrength: filterStrength,
                hasDiscontinuity: hasDiscontinuity
              ) else { return VTB_PARAM_ERR }
        return vtb_process_command_buffer(p, commandBuffer: cb, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_frame_rate_conversion")
public func vt_frame_processor_process_frame_rate_conversion(
    _ processor: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ nextFrame: UnsafeMutableRawPointer?,
    _ opticalFlow: UnsafeMutableRawPointer?,
    _ interpolationPhase: UnsafePointer<Float>?,
    _ interpolationPhaseCount: Int,
    _ submissionMode: Int,
    _ destinationFrames: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ destinationFrameCount: Int
) -> Int32 {
    if #available(macOS 15.4, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let next: VTFrameProcessorFrame? = vtb_borrow_optional(nextFrame)
        let flow: VTFrameProcessorOpticalFlow? = vtb_borrow_optional(opticalFlow)
        guard let next,
              let phases = vtb_numbers(interpolationPhase, count: interpolationPhaseCount),
              let destinations = vtb_borrow_frames(destinationFrames, count: destinationFrameCount),
              let params = VTFrameRateConversionParameters(
                sourceFrame: source,
                nextFrame: next,
                opticalFlow: flow,
                interpolationPhase: phases,
                submissionMode: VTFrameRateConversionParameters.SubmissionMode(rawValue: submissionMode) ?? .random,
                destinationFrames: destinations
              ) else { return VTB_PARAM_ERR }
        return vtb_process_completion(p, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_frame_rate_conversion_with_command_buffer")
public func vt_frame_processor_process_frame_rate_conversion_with_command_buffer(
    _ processor: UnsafeMutableRawPointer,
    _ commandBuffer: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ nextFrame: UnsafeMutableRawPointer?,
    _ opticalFlow: UnsafeMutableRawPointer?,
    _ interpolationPhase: UnsafePointer<Float>?,
    _ interpolationPhaseCount: Int,
    _ submissionMode: Int,
    _ destinationFrames: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ destinationFrameCount: Int
) -> Int32 {
    if #available(macOS 15.4, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        guard let cb = vtb_borrow_command_buffer(commandBuffer) else { return VTB_PARAM_ERR }
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let next: VTFrameProcessorFrame? = vtb_borrow_optional(nextFrame)
        let flow: VTFrameProcessorOpticalFlow? = vtb_borrow_optional(opticalFlow)
        guard let next,
              let phases = vtb_numbers(interpolationPhase, count: interpolationPhaseCount),
              let destinations = vtb_borrow_frames(destinationFrames, count: destinationFrameCount),
              let params = VTFrameRateConversionParameters(
                sourceFrame: source,
                nextFrame: next,
                opticalFlow: flow,
                interpolationPhase: phases,
                submissionMode: VTFrameRateConversionParameters.SubmissionMode(rawValue: submissionMode) ?? .random,
                destinationFrames: destinations
              ) else { return VTB_PARAM_ERR }
        return vtb_process_command_buffer(p, commandBuffer: cb, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_low_latency_super_resolution")
public func vt_frame_processor_process_low_latency_super_resolution(
    _ processor: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ destinationFrame: UnsafeMutableRawPointer
) -> Int32 {
    if #available(macOS 26.0, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let destination: VTFrameProcessorFrame = vtb_borrow(destinationFrame)
        let params = VTLowLatencySuperResolutionScalerParameters(
            sourceFrame: source,
            destinationFrame: destination
        )
        return vtb_process_completion(p, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_low_latency_super_resolution_with_command_buffer")
public func vt_frame_processor_process_low_latency_super_resolution_with_command_buffer(
    _ processor: UnsafeMutableRawPointer,
    _ commandBuffer: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ destinationFrame: UnsafeMutableRawPointer
) -> Int32 {
    if #available(macOS 26.0, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        guard let cb = vtb_borrow_command_buffer(commandBuffer) else { return VTB_PARAM_ERR }
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let destination: VTFrameProcessorFrame = vtb_borrow(destinationFrame)
        let params = VTLowLatencySuperResolutionScalerParameters(
            sourceFrame: source,
            destinationFrame: destination
        )
        return vtb_process_command_buffer(p, commandBuffer: cb, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_low_latency_frame_interpolation")
public func vt_frame_processor_process_low_latency_frame_interpolation(
    _ processor: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ previousFrame: UnsafeMutableRawPointer,
    _ interpolationPhase: UnsafePointer<Float>?,
    _ interpolationPhaseCount: Int,
    _ destinationFrames: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ destinationFrameCount: Int
) -> Int32 {
    if #available(macOS 26.0, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let previous: VTFrameProcessorFrame = vtb_borrow(previousFrame)
        guard let phases = vtb_numbers(interpolationPhase, count: interpolationPhaseCount),
              let destinations = vtb_borrow_frames(destinationFrames, count: destinationFrameCount),
              let params = VTLowLatencyFrameInterpolationParameters(
                sourceFrame: source,
                previousFrame: previous,
                interpolationPhase: phases,
                destinationFrames: destinations
              ) else { return VTB_PARAM_ERR }
        return vtb_process_completion(p, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_low_latency_frame_interpolation_with_command_buffer")
public func vt_frame_processor_process_low_latency_frame_interpolation_with_command_buffer(
    _ processor: UnsafeMutableRawPointer,
    _ commandBuffer: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ previousFrame: UnsafeMutableRawPointer,
    _ interpolationPhase: UnsafePointer<Float>?,
    _ interpolationPhaseCount: Int,
    _ destinationFrames: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ destinationFrameCount: Int
) -> Int32 {
    if #available(macOS 26.0, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        guard let cb = vtb_borrow_command_buffer(commandBuffer) else { return VTB_PARAM_ERR }
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let previous: VTFrameProcessorFrame = vtb_borrow(previousFrame)
        guard let phases = vtb_numbers(interpolationPhase, count: interpolationPhaseCount),
              let destinations = vtb_borrow_frames(destinationFrames, count: destinationFrameCount),
              let params = VTLowLatencyFrameInterpolationParameters(
                sourceFrame: source,
                previousFrame: previous,
                interpolationPhase: phases,
                destinationFrames: destinations
              ) else { return VTB_PARAM_ERR }
        return vtb_process_command_buffer(p, commandBuffer: cb, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_optical_flow")
public func vt_frame_processor_process_optical_flow(
    _ processor: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ nextFrame: UnsafeMutableRawPointer,
    _ submissionMode: Int,
    _ destinationOpticalFlow: UnsafeMutableRawPointer
) -> Int32 {
    if #available(macOS 15.4, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let next: VTFrameProcessorFrame = vtb_borrow(nextFrame)
        let destination: VTFrameProcessorOpticalFlow = vtb_borrow(destinationOpticalFlow)
        guard let params = VTOpticalFlowParameters(
            sourceFrame: source,
            nextFrame: next,
            submissionMode: VTOpticalFlowParameters.SubmissionMode(rawValue: submissionMode) ?? .random,
            destinationOpticalFlow: destination
        ) else { return VTB_PARAM_ERR }
        return vtb_process_completion(p, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

@_cdecl("vt_frame_processor_process_optical_flow_with_command_buffer")
public func vt_frame_processor_process_optical_flow_with_command_buffer(
    _ processor: UnsafeMutableRawPointer,
    _ commandBuffer: UnsafeMutableRawPointer,
    _ sourceFrame: UnsafeMutableRawPointer,
    _ nextFrame: UnsafeMutableRawPointer,
    _ submissionMode: Int,
    _ destinationOpticalFlow: UnsafeMutableRawPointer
) -> Int32 {
    if #available(macOS 15.4, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        guard let cb = vtb_borrow_command_buffer(commandBuffer) else { return VTB_PARAM_ERR }
        let source: VTFrameProcessorFrame = vtb_borrow(sourceFrame)
        let next: VTFrameProcessorFrame = vtb_borrow(nextFrame)
        let destination: VTFrameProcessorOpticalFlow = vtb_borrow(destinationOpticalFlow)
        guard let params = VTOpticalFlowParameters(
            sourceFrame: source,
            nextFrame: next,
            submissionMode: VTOpticalFlowParameters.SubmissionMode(rawValue: submissionMode) ?? .random,
            destinationOpticalFlow: destination
        ) else { return VTB_PARAM_ERR }
        return vtb_process_command_buffer(p, commandBuffer: cb, parameters: params)
    }
    return VTB_NOT_SUPPORTED
}

// MARK: - Session lifecycle

@_cdecl("vt_frame_processor_end")
public func vt_frame_processor_end(_ processor: UnsafeMutableRawPointer) {
    if #available(macOS 15.4, *) {
        let p: VTFrameProcessor = vtb_borrow(processor)
        p.endSession()
    }
}

@_cdecl("vt_frame_processor_release")
public func vt_frame_processor_release(_ processor: UnsafeMutableRawPointer) {
    if #available(macOS 15.4, *) {
        vtb_release(processor, as: VTFrameProcessor.self)
    }
}
