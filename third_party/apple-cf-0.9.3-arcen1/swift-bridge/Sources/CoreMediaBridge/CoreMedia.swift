// CoreMedia Bridge for apple-cf-rs.
//
// Generic, framework-agnostic CMSampleBuffer / CMBlockBuffer /
// CMFormatDescription accessors plus audio buffer list bridging. Keeps the
// surface here intentionally small — anything that depends on
// ScreenCaptureKit-specific attachment keys (SCStreamFrameInfo) lives in
// screencapturekit-rs's own bridge.

import CoreMedia
import CoreVideo
import Foundation

// MARK: - Audio Buffer List bridging
//
// CoreAudio's AudioBufferList is a variable-sized struct (mNumberBuffers
// followed by inlined AudioBuffers) which Rust cannot represent directly
// without macro tricks. We expose a fixed-size POD record and a heap
// pointer to a Rust-friendly array.

public struct AudioBufferBridge {
    public var number_channels: UInt32
    public var data_bytes_size: UInt32
    public var data_ptr: UnsafeMutableRawPointer?
}

public struct AudioBufferListRaw {
    public var num_buffers: UInt32
    public var buffers_ptr: UnsafeMutablePointer<AudioBufferBridge>?
    public var buffers_len: UInt
}

// MARK: - CMSampleBuffer (generic accessors)

@_cdecl("cm_sample_buffer_release")
public func cm_sample_buffer_release(_ buffer: UnsafeMutableRawPointer) {
    Unmanaged<CMSampleBuffer>.fromOpaque(buffer).release()
}

@_cdecl("cm_sample_buffer_retain")
public func cm_sample_buffer_retain(_ buffer: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    return Unmanaged.passRetained(buf).toOpaque()
}

@_cdecl("cm_sample_buffer_hash")
public func cm_sample_buffer_hash(_ buffer: UnsafeMutableRawPointer) -> Int {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    return ObjectIdentifier(buf).hashValue
}

@_cdecl("cm_sample_buffer_get_data_buffer")
public func cm_sample_buffer_get_data_buffer(
    _ buffer: UnsafeMutableRawPointer
) -> UnsafeMutableRawPointer? {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    guard let bb = CMSampleBufferGetDataBuffer(buf) else { return nil }
    // Return an unretained reference; Rust side calls cm_block_buffer_retain
    // before wrapping if it wants to hold on.
    return Unmanaged.passUnretained(bb).toOpaque()
}

@_cdecl("cm_sample_buffer_get_format_description")
public func cm_sample_buffer_get_format_description(
    _ buffer: UnsafeMutableRawPointer
) -> UnsafeMutableRawPointer? {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    guard let fd = CMSampleBufferGetFormatDescription(buf) else { return nil }
    return Unmanaged.passUnretained(fd).toOpaque()
}

@_cdecl("cm_sample_buffer_get_image_buffer")
public func cm_sample_buffer_get_image_buffer(
    _ buffer: UnsafeMutableRawPointer
) -> UnsafeMutableRawPointer? {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    guard let img = CMSampleBufferGetImageBuffer(buf) else { return nil }
    return Unmanaged.passRetained(img).toOpaque()
}

private func writeCMTime(
    _ time: CMTime,
    _ outValue: UnsafeMutablePointer<Int64>,
    _ outTimescale: UnsafeMutablePointer<Int32>,
    _ outFlags: UnsafeMutablePointer<UInt32>,
    _ outEpoch: UnsafeMutablePointer<Int64>
) {
    outValue.pointee = time.value
    outTimescale.pointee = time.timescale
    outFlags.pointee = time.flags.rawValue
    outEpoch.pointee = time.epoch
}

@_cdecl("cm_sample_buffer_get_presentation_timestamp")
public func cm_sample_buffer_get_presentation_timestamp(
    _ buffer: UnsafeMutableRawPointer,
    _ outValue: UnsafeMutablePointer<Int64>,
    _ outTimescale: UnsafeMutablePointer<Int32>,
    _ outFlags: UnsafeMutablePointer<UInt32>,
    _ outEpoch: UnsafeMutablePointer<Int64>
) {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    writeCMTime(CMSampleBufferGetPresentationTimeStamp(buf), outValue, outTimescale, outFlags, outEpoch)
}

@_cdecl("cm_sample_buffer_get_decode_timestamp")
public func cm_sample_buffer_get_decode_timestamp(
    _ buffer: UnsafeMutableRawPointer,
    _ outValue: UnsafeMutablePointer<Int64>,
    _ outTimescale: UnsafeMutablePointer<Int32>,
    _ outFlags: UnsafeMutablePointer<UInt32>,
    _ outEpoch: UnsafeMutablePointer<Int64>
) {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    writeCMTime(CMSampleBufferGetDecodeTimeStamp(buf), outValue, outTimescale, outFlags, outEpoch)
}

@_cdecl("cm_sample_buffer_get_duration")
public func cm_sample_buffer_get_duration(
    _ buffer: UnsafeMutableRawPointer,
    _ outValue: UnsafeMutablePointer<Int64>,
    _ outTimescale: UnsafeMutablePointer<Int32>,
    _ outFlags: UnsafeMutablePointer<UInt32>,
    _ outEpoch: UnsafeMutablePointer<Int64>
) {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    writeCMTime(CMSampleBufferGetDuration(buf), outValue, outTimescale, outFlags, outEpoch)
}

@_cdecl("cm_sample_buffer_get_num_samples")
public func cm_sample_buffer_get_num_samples(_ buffer: UnsafeMutableRawPointer) -> Int64 {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    return Int64(CMSampleBufferGetNumSamples(buf))
}

@_cdecl("cm_sample_buffer_is_valid")
public func cm_sample_buffer_is_valid(_ buffer: UnsafeMutableRawPointer) -> Bool {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    return CMSampleBufferIsValid(buf)
}

@_cdecl("cm_sample_buffer_data_is_ready")
public func cm_sample_buffer_data_is_ready(_ buffer: UnsafeMutableRawPointer) -> Bool {
    let buf = Unmanaged<CMSampleBuffer>.fromOpaque(buffer).takeUnretainedValue()
    return CMSampleBufferDataIsReady(buf)
}

// MARK: - CMBlockBuffer

@_cdecl("cm_block_buffer_release")
public func cm_block_buffer_release(_ blockBuffer: UnsafeMutableRawPointer) {
    Unmanaged<CMBlockBuffer>.fromOpaque(blockBuffer).release()
}

@_cdecl("cm_block_buffer_retain")
public func cm_block_buffer_retain(_ blockBuffer: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer {
    let bb = Unmanaged<CMBlockBuffer>.fromOpaque(blockBuffer).takeUnretainedValue()
    return Unmanaged.passRetained(bb).toOpaque()
}

@_cdecl("cm_block_buffer_hash")
public func cm_block_buffer_hash(_ blockBuffer: UnsafeMutableRawPointer) -> Int {
    let bb = Unmanaged<CMBlockBuffer>.fromOpaque(blockBuffer).takeUnretainedValue()
    return ObjectIdentifier(bb).hashValue
}

@_cdecl("cm_block_buffer_get_data_length")
public func cm_block_buffer_get_data_length(_ blockBuffer: UnsafeMutableRawPointer) -> Int {
    let bb = Unmanaged<CMBlockBuffer>.fromOpaque(blockBuffer).takeUnretainedValue()
    return CMBlockBufferGetDataLength(bb)
}

@_cdecl("cm_block_buffer_is_empty")
public func cm_block_buffer_is_empty(_ blockBuffer: UnsafeMutableRawPointer) -> Bool {
    let bb = Unmanaged<CMBlockBuffer>.fromOpaque(blockBuffer).takeUnretainedValue()
    return CMBlockBufferIsEmpty(bb)
}

@_cdecl("cm_block_buffer_is_range_contiguous")
public func cm_block_buffer_is_range_contiguous(
    _ blockBuffer: UnsafeMutableRawPointer,
    _ offset: Int,
    _ length: Int
) -> Bool {
    let bb = Unmanaged<CMBlockBuffer>.fromOpaque(blockBuffer).takeUnretainedValue()
    return CMBlockBufferIsRangeContiguous(bb, atOffset: offset, length: length)
}

@_cdecl("cm_block_buffer_get_data_pointer")
public func cm_block_buffer_get_data_pointer(
    _ blockBuffer: UnsafeMutableRawPointer,
    _ offset: Int,
    _ outLengthAtOffset: UnsafeMutablePointer<Int>?,
    _ outTotalLength: UnsafeMutablePointer<Int>?,
    _ outDataPointer: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    let bb = Unmanaged<CMBlockBuffer>.fromOpaque(blockBuffer).takeUnretainedValue()
    var dataPtr: UnsafeMutablePointer<CChar>?
    let status = CMBlockBufferGetDataPointer(
        bb,
        atOffset: offset,
        lengthAtOffsetOut: outLengthAtOffset,
        totalLengthOut: outTotalLength,
        dataPointerOut: &dataPtr
    )
    outDataPointer.pointee = dataPtr.map { UnsafeMutableRawPointer($0) }
    return status
}

@_cdecl("cm_block_buffer_copy_data_bytes")
public func cm_block_buffer_copy_data_bytes(
    _ blockBuffer: UnsafeMutableRawPointer,
    _ offset: Int,
    _ dataLength: Int,
    _ destination: UnsafeMutableRawPointer
) -> Int32 {
    let bb = Unmanaged<CMBlockBuffer>.fromOpaque(blockBuffer).takeUnretainedValue()
    return CMBlockBufferCopyDataBytes(
        bb,
        atOffset: offset,
        dataLength: dataLength,
        destination: destination
    )
}

@_cdecl("cm_block_buffer_create_with_data")
public func cm_block_buffer_create_with_data(
    _ data: UnsafeRawPointer,
    _ dataLength: Int,
    _ blockBufferOut: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    var bb: CMBlockBuffer?
    let status = CMBlockBufferCreateWithMemoryBlock(
        allocator: kCFAllocatorDefault,
        memoryBlock: nil,
        blockLength: dataLength,
        blockAllocator: kCFAllocatorDefault,
        customBlockSource: nil,
        offsetToData: 0,
        dataLength: dataLength,
        flags: kCMBlockBufferAssureMemoryNowFlag,
        blockBufferOut: &bb
    )
    if status != noErr { return status }
    guard let bb = bb else { return -1 }
    let copyStatus = CMBlockBufferReplaceDataBytes(
        with: data,
        blockBuffer: bb,
        offsetIntoDestination: 0,
        dataLength: dataLength
    )
    if copyStatus != noErr { return copyStatus }
    blockBufferOut.pointee = Unmanaged.passRetained(bb).toOpaque()
    return noErr
}

@_cdecl("cm_block_buffer_create_empty")
public func cm_block_buffer_create_empty(
    _ blockBufferOut: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    var bb: CMBlockBuffer?
    let status = CMBlockBufferCreateEmpty(
        allocator: kCFAllocatorDefault,
        capacity: 0,
        flags: 0,
        blockBufferOut: &bb
    )
    if status != noErr { return status }
    guard let bb = bb else { return -1 }
    blockBufferOut.pointee = Unmanaged.passRetained(bb).toOpaque()
    return noErr
}

// MARK: - CMFormatDescription

@_cdecl("cm_format_description_release")
public func cm_format_description_release(_ formatDescription: UnsafeMutableRawPointer) {
    Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).release()
}

@_cdecl("cm_format_description_retain")
public func cm_format_description_retain(_ formatDescription: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    return Unmanaged.passRetained(fd).toOpaque()
}

@_cdecl("cm_format_description_hash")
public func cm_format_description_hash(_ formatDescription: UnsafeMutableRawPointer) -> Int {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    return ObjectIdentifier(fd).hashValue
}

@_cdecl("cm_format_description_get_media_type")
public func cm_format_description_get_media_type(_ formatDescription: UnsafeMutableRawPointer) -> UInt32 {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    return CMFormatDescriptionGetMediaType(fd)
}

@_cdecl("cm_format_description_get_media_subtype")
public func cm_format_description_get_media_subtype(_ formatDescription: UnsafeMutableRawPointer) -> UInt32 {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    return CMFormatDescriptionGetMediaSubType(fd)
}

@_cdecl("cm_format_description_get_extensions")
public func cm_format_description_get_extensions(
    _ formatDescription: UnsafeMutableRawPointer
) -> UnsafeRawPointer? {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    guard let exts = CMFormatDescriptionGetExtensions(fd) else { return nil }
    return UnsafeRawPointer(Unmanaged.passUnretained(exts).toOpaque())
}

@_cdecl("cm_format_description_get_audio_sample_rate")
public func cm_format_description_get_audio_sample_rate(
    _ formatDescription: UnsafeMutableRawPointer
) -> Float64 {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    guard let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fd) else { return 0 }
    return asbd.pointee.mSampleRate
}

@_cdecl("cm_format_description_get_audio_channel_count")
public func cm_format_description_get_audio_channel_count(
    _ formatDescription: UnsafeMutableRawPointer
) -> UInt32 {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    guard let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fd) else { return 0 }
    return asbd.pointee.mChannelsPerFrame
}

@_cdecl("cm_format_description_get_audio_bits_per_channel")
public func cm_format_description_get_audio_bits_per_channel(
    _ formatDescription: UnsafeMutableRawPointer
) -> UInt32 {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    guard let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fd) else { return 0 }
    return asbd.pointee.mBitsPerChannel
}

@_cdecl("cm_format_description_get_audio_bytes_per_frame")
public func cm_format_description_get_audio_bytes_per_frame(
    _ formatDescription: UnsafeMutableRawPointer
) -> UInt32 {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    guard let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fd) else { return 0 }
    return asbd.pointee.mBytesPerFrame
}

@_cdecl("cm_format_description_get_audio_format_flags")
public func cm_format_description_get_audio_format_flags(
    _ formatDescription: UnsafeMutableRawPointer
) -> UInt32 {
    let fd = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    guard let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fd) else { return 0 }
    return asbd.pointee.mFormatFlags
}

private func acfRetainedCFStringConstant(_ value: CFString) -> UnsafeMutableRawPointer {
    Unmanaged.passRetained(value).toOpaque()
}

// MARK: - CMMetadataFormatDescription

@_cdecl("cm_metadata_format_description_create_with_keys")
public func cm_metadata_format_description_create_with_keys(
    _ metadataType: UInt32,
    _ keys: UnsafeMutableRawPointer?,
    _ formatDescriptionOut: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    let keys = keys.map { Unmanaged<CFArray>.fromOpaque($0).takeUnretainedValue() }
    var formatDescription: CMFormatDescription?
    let status = CMMetadataFormatDescriptionCreateWithKeys(
        allocator: nil,
        metadataType: metadataType,
        keys: keys,
        formatDescriptionOut: &formatDescription
    )
    if status == noErr, let formatDescription {
        formatDescriptionOut.pointee = Unmanaged.passRetained(formatDescription).toOpaque()
    } else {
        formatDescriptionOut.pointee = nil
    }
    return status
}

@_cdecl("cm_metadata_format_description_create_with_metadata_specifications")
public func cm_metadata_format_description_create_with_metadata_specifications(
    _ metadataType: UInt32,
    _ metadataSpecifications: UnsafeMutableRawPointer,
    _ formatDescriptionOut: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    let metadataSpecifications = Unmanaged<CFArray>.fromOpaque(metadataSpecifications).takeUnretainedValue()
    var formatDescription: CMFormatDescription?
    let status = CMMetadataFormatDescriptionCreateWithMetadataSpecifications(
        allocator: nil,
        metadataType: metadataType,
        metadataSpecifications: metadataSpecifications,
        formatDescriptionOut: &formatDescription
    )
    if status == noErr, let formatDescription {
        formatDescriptionOut.pointee = Unmanaged.passRetained(formatDescription).toOpaque()
    } else {
        formatDescriptionOut.pointee = nil
    }
    return status
}

@_cdecl("cm_metadata_format_description_create_with_description_and_metadata_specifications")
public func cm_metadata_format_description_create_with_description_and_metadata_specifications(
    _ sourceDescription: UnsafeMutableRawPointer,
    _ metadataSpecifications: UnsafeMutableRawPointer,
    _ formatDescriptionOut: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    let sourceDescription = Unmanaged<CMFormatDescription>.fromOpaque(sourceDescription).takeUnretainedValue()
    let metadataSpecifications = Unmanaged<CFArray>.fromOpaque(metadataSpecifications).takeUnretainedValue()
    var formatDescription: CMFormatDescription?
    let status = CMMetadataFormatDescriptionCreateWithMetadataFormatDescriptionAndMetadataSpecifications(
        allocator: nil,
        sourceDescription: sourceDescription,
        metadataSpecifications: metadataSpecifications,
        formatDescriptionOut: &formatDescription
    )
    if status == noErr, let formatDescription {
        formatDescriptionOut.pointee = Unmanaged.passRetained(formatDescription).toOpaque()
    } else {
        formatDescriptionOut.pointee = nil
    }
    return status
}

@_cdecl("cm_metadata_format_description_create_by_merging_descriptions")
public func cm_metadata_format_description_create_by_merging_descriptions(
    _ sourceDescription: UnsafeMutableRawPointer,
    _ otherSourceDescription: UnsafeMutableRawPointer,
    _ formatDescriptionOut: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Int32 {
    let sourceDescription = Unmanaged<CMFormatDescription>.fromOpaque(sourceDescription).takeUnretainedValue()
    let otherSourceDescription = Unmanaged<CMFormatDescription>.fromOpaque(otherSourceDescription).takeUnretainedValue()
    var formatDescription: CMFormatDescription?
    let status = CMMetadataFormatDescriptionCreateByMergingMetadataFormatDescriptions(
        allocator: nil,
        sourceDescription: sourceDescription,
        otherSourceDescription: otherSourceDescription,
        formatDescriptionOut: &formatDescription
    )
    if status == noErr, let formatDescription {
        formatDescriptionOut.pointee = Unmanaged.passRetained(formatDescription).toOpaque()
    } else {
        formatDescriptionOut.pointee = nil
    }
    return status
}

@_cdecl("cm_metadata_format_description_get_identifiers")
public func cm_metadata_format_description_get_identifiers(
    _ formatDescription: UnsafeMutableRawPointer
) -> UnsafeMutableRawPointer? {
    let formatDescription = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    guard let identifiers = CMMetadataFormatDescriptionGetIdentifiers(formatDescription) else {
        return nil
    }
    return Unmanaged.passRetained(identifiers).toOpaque()
}

@_cdecl("cm_metadata_format_description_get_key_with_local_id")
public func cm_metadata_format_description_get_key_with_local_id(
    _ formatDescription: UnsafeMutableRawPointer,
    _ localID: UInt32
) -> UnsafeMutableRawPointer? {
    let formatDescription = Unmanaged<CMFormatDescription>.fromOpaque(formatDescription).takeUnretainedValue()
    guard let key = CMMetadataFormatDescriptionGetKeyWithLocalID(formatDescription, localKeyID: localID) else {
        return nil
    }
    return Unmanaged.passRetained(key).toOpaque()
}

@_cdecl("cm_metadata_format_description_extension_key_metadata_key_table")
public func cm_metadata_format_description_extension_key_metadata_key_table() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMFormatDescriptionExtensionKey_MetadataKeyTable)
}

@_cdecl("cm_metadata_format_description_key_conforming_data_types")
public func cm_metadata_format_description_key_conforming_data_types() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_ConformingDataTypes)
}

@_cdecl("cm_metadata_format_description_key_data_type")
public func cm_metadata_format_description_key_data_type() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_DataType)
}

@_cdecl("cm_metadata_format_description_key_data_type_namespace")
public func cm_metadata_format_description_key_data_type_namespace() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_DataTypeNamespace)
}

@_cdecl("cm_metadata_format_description_key_language_tag")
public func cm_metadata_format_description_key_language_tag() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_LanguageTag)
}

@_cdecl("cm_metadata_format_description_key_local_id")
public func cm_metadata_format_description_key_local_id() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_LocalID)
}

@_cdecl("cm_metadata_format_description_key_namespace")
public func cm_metadata_format_description_key_namespace() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_Namespace)
}

@_cdecl("cm_metadata_format_description_key_setup_data")
public func cm_metadata_format_description_key_setup_data() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_SetupData)
}

@_cdecl("cm_metadata_format_description_key_structural_dependency")
public func cm_metadata_format_description_key_structural_dependency() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_StructuralDependency)
}

@_cdecl("cm_metadata_format_description_key_value")
public func cm_metadata_format_description_key_value() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionKey_Value)
}

@_cdecl("cm_metadata_format_description_metadata_specification_key_data_type")
public func cm_metadata_format_description_metadata_specification_key_data_type() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionMetadataSpecificationKey_DataType)
}

@_cdecl("cm_metadata_format_description_metadata_specification_key_extended_language_tag")
public func cm_metadata_format_description_metadata_specification_key_extended_language_tag() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionMetadataSpecificationKey_ExtendedLanguageTag)
}

@_cdecl("cm_metadata_format_description_metadata_specification_key_identifier")
public func cm_metadata_format_description_metadata_specification_key_identifier() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionMetadataSpecificationKey_Identifier)
}

@_cdecl("cm_metadata_format_description_metadata_specification_key_setup_data")
public func cm_metadata_format_description_metadata_specification_key_setup_data() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionMetadataSpecificationKey_SetupData)
}

@_cdecl("cm_metadata_format_description_metadata_specification_key_structural_dependency")
public func cm_metadata_format_description_metadata_specification_key_structural_dependency() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescriptionMetadataSpecificationKey_StructuralDependency)
}

@_cdecl("cm_metadata_format_description_structural_dependency_key_dependency_is_invalid_flag")
public func cm_metadata_format_description_structural_dependency_key_dependency_is_invalid_flag() -> UnsafeMutableRawPointer {
    acfRetainedCFStringConstant(kCMMetadataFormatDescription_StructuralDependencyKey_DependencyIsInvalidFlag)
}
