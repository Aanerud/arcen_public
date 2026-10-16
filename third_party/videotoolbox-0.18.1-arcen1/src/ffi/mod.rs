//! Raw `extern "C"` declarations against `<VideoToolbox/VideoToolbox.h>`,
//! `<CoreMedia/CoreMedia.h>`, `<CoreVideo/CVPixelBuffer.h>`, and
//! `<CoreFoundation/CoreFoundation.h>`.
//!
//! These are intentionally `unsafe` and opaque (`*const c_void` / `*mut c_void`).
//! Safe wrappers live in [`crate::compression`] and [`crate::session`].

#![allow(missing_docs, non_camel_case_types, non_upper_case_globals)]

use core::ffi::{c_char, c_uint, c_void};

// ---- type aliases that match the C headers ----

pub use apple_cf::raw::OSStatus;

pub use apple_cf::cm::{CMTime, CMTimeRange};

// CMVideoCodecType FourCC codes (ASCII bytes).
pub const kCMVideoCodecType_H264: CMVideoCodecType = u32::from_be_bytes(*b"avc1");
pub const kCMVideoCodecType_HEVC: CMVideoCodecType = u32::from_be_bytes(*b"hvc1");
pub const kCMVideoCodecType_AppleProRes422: CMVideoCodecType = u32::from_be_bytes(*b"apcn");
pub const kCMVideoCodecType_AppleProRes422HQ: CMVideoCodecType = u32::from_be_bytes(*b"apch");
pub const kCMVideoCodecType_AppleProRes422LT: CMVideoCodecType = u32::from_be_bytes(*b"apcs");
pub const kCMVideoCodecType_AppleProRes422Proxy: CMVideoCodecType = u32::from_be_bytes(*b"apco");
pub const kCMVideoCodecType_AppleProRes4444: CMVideoCodecType = u32::from_be_bytes(*b"ap4h");

// VTEncodeInfoFlags (returned via the encode callback)
pub const kVTEncodeInfo_Asynchronous: u32 = 1;
pub const kVTEncodeInfo_FrameDropped: u32 = 1 << 1;
pub const kVTCompressionSessionBeginFinalPass: VTCompressionSessionOptionFlags = 1 << 0;
pub const kVTDecodeFrame_EnableAsynchronousDecompression: VTDecodeFrameFlags = 1 << 0;
pub const kVTDecodeFrame_DoNotOutputFrame: VTDecodeFrameFlags = 1 << 1;
pub const kVTDecodeFrame_1xRealTimePlayback: VTDecodeFrameFlags = 1 << 2;
pub const kVTDecodeFrame_EnableTemporalProcessing: VTDecodeFrameFlags = 1 << 3;
pub const kVTDecodeInfo_Asynchronous: VTDecodeInfoFlags = 1 << 0;
pub const kVTDecodeInfo_FrameDropped: VTDecodeInfoFlags = 1 << 1;
pub const kVTDecodeInfo_ImageBufferModifiable: VTDecodeInfoFlags = 1 << 2;
pub const kVTDecodeInfo_SkippedLeadingFrameDropped: VTDecodeInfoFlags = 1 << 3;
pub const kVTDecodeInfo_FrameInterrupted: VTDecodeInfoFlags = 1 << 4;

pub type VTFrameRateConversionConfigurationQualityPrioritization = isize;
pub const VTFrameRateConversionConfigurationQualityPrioritizationNormal:
    VTFrameRateConversionConfigurationQualityPrioritization = 1;
pub const VTFrameRateConversionConfigurationQualityPrioritizationQuality:
    VTFrameRateConversionConfigurationQualityPrioritization = 2;
pub type VTFrameRateConversionConfigurationRevision = isize;
pub const VTFrameRateConversionConfigurationRevision1: VTFrameRateConversionConfigurationRevision =
    1;
pub type VTMotionBlurConfigurationQualityPrioritization = isize;
pub const VTMotionBlurConfigurationQualityPrioritizationNormal:
    VTMotionBlurConfigurationQualityPrioritization = 1;
pub const VTMotionBlurConfigurationQualityPrioritizationQuality:
    VTMotionBlurConfigurationQualityPrioritization = 2;
pub type VTMotionBlurConfigurationRevision = isize;
pub const VTMotionBlurConfigurationRevision1: VTMotionBlurConfigurationRevision = 1;
pub type VTOpticalFlowConfigurationQualityPrioritization = isize;
pub const VTOpticalFlowConfigurationQualityPrioritizationNormal:
    VTOpticalFlowConfigurationQualityPrioritization = 1;
pub const VTOpticalFlowConfigurationQualityPrioritizationQuality:
    VTOpticalFlowConfigurationQualityPrioritization = 2;
pub type VTOpticalFlowConfigurationRevision = isize;
pub const VTOpticalFlowConfigurationRevision1: VTOpticalFlowConfigurationRevision = 1;
pub type VTSuperResolutionScalerConfigurationInputType = isize;
pub const VTSuperResolutionScalerConfigurationInputTypeVideo:
    VTSuperResolutionScalerConfigurationInputType = 1;
pub const VTSuperResolutionScalerConfigurationInputTypeImage:
    VTSuperResolutionScalerConfigurationInputType = 2;
pub type VTSuperResolutionScalerConfigurationQualityPrioritization = isize;
pub const VTSuperResolutionScalerConfigurationQualityPrioritizationNormal:
    VTSuperResolutionScalerConfigurationQualityPrioritization = 1;
pub type VTSuperResolutionScalerConfigurationRevision = isize;
pub const VTSuperResolutionScalerConfigurationRevision1:
    VTSuperResolutionScalerConfigurationRevision = 1;
pub type VTMotionEstimationFrameFlags = u32;
pub const kVTMotionEstimationFrameFlags_CurrentBufferWillBeNextReferenceBuffer:
    VTMotionEstimationFrameFlags = 1 << 0;
pub type VTMotionEstimationInfoFlags = u32;
pub const kVTMotionEstimationInfoFlags_Reserved0: VTMotionEstimationInfoFlags = 1 << 0;

// ---- CoreFoundation minimum required surface ----

pub use apple_cf::raw::{
    CFAllocatorRef, CFArrayRef, CFBooleanRef, CFDictionaryRef, CFMutableDictionaryRef, CFNumberRef,
    CFNumberType, CFStringRef, CFTypeRef, CFURLRef, CMBlockBufferRef, CMFormatDescriptionRef,
    CMItemCount, CMSampleBufferRef, CMTaggedBufferGroupRef, CMTimeFlags, CMVideoCodecType,
};
pub type VTExtensionPropertiesKey = CFStringRef;
pub type VTHDRPerFrameMetadataGenerationHDRFormatType = CFStringRef;
pub const kCFNumberSInt32Type: CFNumberType = 3;
pub const kCFNumberFloat64Type: CFNumberType = 13;

// CFNumberFormatter etc. — not needed for the encoder API.
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

extern "C" {
    pub static kCFAllocatorDefault: CFAllocatorRef;
    pub static kCFBooleanTrue: CFBooleanRef;
    pub static kCFBooleanFalse: CFBooleanRef;
    pub static kCFTypeDictionaryKeyCallBacks: c_void;
    pub static kCFTypeDictionaryValueCallBacks: c_void;

    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;

    pub fn CFArrayGetCount(array: CFArrayRef) -> isize;
    pub fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> *const c_void;
    pub fn CFDictionaryGetValue(d: CFDictionaryRef, key: *const c_void) -> *const c_void;
    pub fn CFStringGetCString(
        s: CFStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    pub fn CFStringGetLength(s: CFStringRef) -> isize;
    pub fn CFNumberGetValue(
        num: CFNumberRef,
        the_type: CFNumberType,
        value_ptr: *mut c_void,
    ) -> bool;

    pub fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: CFNumberType,
        value_ptr: *const c_void,
    ) -> CFNumberRef;

    pub fn CFStringCreateWithCString(
        allocator: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;

    pub fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    pub fn CFDictionarySetValue(
        dict: CFMutableDictionaryRef,
        key: *const c_void,
        value: *const c_void,
    );
}

// ---- CoreMedia: CMSampleBuffer ----

pub use apple_cf::raw::Boolean;

extern "C" {
    pub fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
    pub fn CMSampleBufferGetTotalSampleSize(sbuf: CMSampleBufferRef) -> usize;
    pub fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
    pub fn CMSampleBufferGetPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
    pub fn CMSampleBufferGetDuration(sbuf: CMSampleBufferRef) -> CMTime;
    pub fn CMBlockBufferCopyDataBytes(
        the_source_buffer: CMBlockBufferRef,
        offset_to_data: usize,
        data_length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    pub fn CMBlockBufferGetDataLength(the_buffer: CMBlockBufferRef) -> usize;
    pub fn CMTaggedBufferGroupGetTypeID() -> usize;
    pub fn CMTaggedBufferGroupGetCount(group: CMTaggedBufferGroupRef) -> CMItemCount;
    pub fn CMTaggedBufferGroupGetCVPixelBufferAtIndex(
        group: CMTaggedBufferGroupRef,
        index: isize,
    ) -> CVPixelBufferRef;
    pub fn CMTaggedBufferGroupGetCMSampleBufferAtIndex(
        group: CMTaggedBufferGroupRef,
        index: isize,
    ) -> CMSampleBufferRef;
}

// ---- CoreVideo: CVPixelBuffer (we only need create-with-IOSurface here) ----

pub use apple_cf::raw::{CVPixelBufferPoolRef, CVPixelBufferRef};
pub type IOSurfaceRef = *mut c_void;

extern "C" {
    pub fn CVPixelBufferCreateWithIOSurface(
        allocator: CFAllocatorRef,
        surface: IOSurfaceRef,
        pixel_buffer_attributes: CFDictionaryRef,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> i32;
}

// ---- VideoToolbox: VTCompressionSession ----

pub type VTSessionRef = *mut c_void;
pub type VTCompressionSessionRef = *mut c_void;
pub type VTCompressionSessionOptionFlags = u32;
pub type VTEncodeInfoFlags = u32;

/// Encode-completion callback signature. Invoked once per encoded frame.
pub type VTCompressionOutputCallback = unsafe extern "C" fn(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: VTEncodeInfoFlags,
    sample_buffer: CMSampleBufferRef,
);

extern "C" {
    pub fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: CMVideoCodecType,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: Option<VTCompressionOutputCallback>,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;

    pub fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
    pub fn VTCompressionSessionGetTypeID() -> usize;
    pub fn VTCompressionSessionGetPixelBufferPool(
        session: VTCompressionSessionRef,
    ) -> CVPixelBufferPoolRef;

    pub fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;

    pub fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVPixelBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut VTEncodeInfoFlags,
    ) -> OSStatus;
    pub fn VTCompressionSessionEncodeFrameWithOutputHandler(
        session: VTCompressionSessionRef,
        image_buffer: CVPixelBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        info_flags_out: *mut VTEncodeInfoFlags,
        output_handler: *const c_void,
    ) -> OSStatus;
    pub fn VTCompressionSessionEncodeMultiImageFrame(
        session: VTCompressionSessionRef,
        tagged_buffer_group: CMTaggedBufferGroupRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut VTEncodeInfoFlags,
    ) -> OSStatus;
    pub fn VTCompressionSessionEncodeMultiImageFrameWithOutputHandler(
        session: VTCompressionSessionRef,
        tagged_buffer_group: CMTaggedBufferGroupRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        info_flags_out: *mut VTEncodeInfoFlags,
        output_handler: *const c_void,
    ) -> OSStatus;
    pub fn VTIsStereoMVHEVCEncodeSupported() -> Boolean;

    pub fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until_presentation_time_stamp: CMTime,
    ) -> OSStatus;
    pub fn VTCompressionSessionBeginPass(
        session: VTCompressionSessionRef,
        begin_pass_flags: VTCompressionSessionOptionFlags,
        reserved: *mut u32,
    ) -> OSStatus;
    pub fn VTCompressionSessionEndPass(
        session: VTCompressionSessionRef,
        further_passes_requested_out: *mut Boolean,
        reserved: *mut u32,
    ) -> OSStatus;
    pub fn VTCompressionSessionGetTimeRangesForNextPass(
        session: VTCompressionSessionRef,
        time_range_count_out: *mut CMItemCount,
        time_range_array_out: *mut *const CMTimeRange,
    ) -> OSStatus;

    pub fn VTSessionSetProperty(
        session: VTSessionRef,
        property_key: CFStringRef,
        property_value: CFTypeRef,
    ) -> OSStatus;
    pub fn VTSessionCopyProperty(
        session: VTSessionRef,
        property_key: CFStringRef,
        allocator: CFAllocatorRef,
        property_value_out: *mut c_void,
    ) -> OSStatus;
    pub fn VTSessionSetProperties(
        session: VTSessionRef,
        property_dictionary: CFDictionaryRef,
    ) -> OSStatus;
    pub fn VTSessionCopySerializableProperties(
        session: VTSessionRef,
        allocator: CFAllocatorRef,
        dictionary_out: *mut CFDictionaryRef,
    ) -> OSStatus;
    pub fn VTSessionCopySupportedPropertyDictionary(
        session: VTSessionRef,
        supported_property_dictionary_out: *mut CFDictionaryRef,
    ) -> OSStatus;
    pub static kVTPropertyTypeKey: CFStringRef;
    pub static kVTPropertyType_Enumeration: CFStringRef;
    pub static kVTPropertyType_Boolean: CFStringRef;
    pub static kVTPropertyType_Number: CFStringRef;
    pub static kVTPropertyReadWriteStatusKey: CFStringRef;
    pub static kVTPropertyReadWriteStatus_ReadOnly: CFStringRef;
    pub static kVTPropertyReadWriteStatus_ReadWrite: CFStringRef;
    pub static kVTPropertyShouldBeSerializedKey: CFStringRef;
    pub static kVTPropertySupportedValueMinimumKey: CFStringRef;
    pub static kVTPropertySupportedValueMaximumKey: CFStringRef;
    pub static kVTPropertySupportedValueListKey: CFStringRef;
    pub static kVTPropertyDocumentationKey: CFStringRef;

    // Common compression property keys (declared as CFStringRef constants by the framework).
    pub static kVTCompressionPropertyKey_RealTime: CFStringRef;
    pub static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    pub static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    pub static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    pub static kVTCompressionPropertyKey_H264EntropyMode: CFStringRef;
    pub static kVTCompressionPropertyKey_Quality: CFStringRef;
    pub static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    pub static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    pub static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    pub static kVTCompressionPropertyKey_ICCProfile: CFStringRef;
    pub static kVTCompressionPropertyKey_MultiPassStorage: CFStringRef;
    pub static kVTVideoEncoderSpecification_EncoderID: CFStringRef;
    pub static kVTAlphaChannelMode_PremultipliedAlpha: CFStringRef;
    pub static kVTAlphaChannelMode_StraightAlpha: CFStringRef;
    pub static kVTCameraCalibrationExtrinsicOriginSource_StereoCameraSystemBaseline: CFStringRef;
    pub static kVTCameraCalibrationLensAlgorithmKind_ParametricLens: CFStringRef;
    pub static kVTCameraCalibrationLensDomain_Color: CFStringRef;
    pub static kVTCameraCalibrationLensRole_Left: CFStringRef;
    pub static kVTCameraCalibrationLensRole_Mono: CFStringRef;
    pub static kVTCameraCalibrationLensRole_Right: CFStringRef;
    pub static kVTCompressionPreset_Balanced: CFStringRef;
    pub static kVTCompressionPreset_HighQuality: CFStringRef;
    pub static kVTCompressionPreset_HighSpeed: CFStringRef;
    pub static kVTCompressionPreset_VideoConferencing: CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_ExtrinsicOrientationQuaternion:
        CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_ExtrinsicOriginSource: CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_IntrinsicMatrix: CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_IntrinsicMatrixProjectionOffset:
        CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_IntrinsicMatrixReferenceDimensions:
        CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_LensAlgorithmKind: CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_LensDistortions: CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_LensDomain: CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_LensFrameAdjustmentsPolynomialX:
        CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_LensFrameAdjustmentsPolynomialY:
        CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_LensIdentifier: CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_LensRole: CFStringRef;
    pub static kVTCompressionPropertyCameraCalibrationKey_RadialAngleLimit: CFStringRef;
    pub static kVTCompressionPropertyKey_AllowOpenGOP: CFStringRef;
    pub static kVTCompressionPropertyKey_AllowTemporalCompression: CFStringRef;
    pub static kVTCompressionPropertyKey_AlphaChannelMode: CFStringRef;
    pub static kVTCompressionPropertyKey_AspectRatio16x9: CFStringRef;
    pub static kVTCompressionPropertyKey_BaseLayerBitRateFraction: CFStringRef;
    pub static kVTCompressionPropertyKey_BaseLayerFrameRate: CFStringRef;
    pub static kVTCompressionPropertyKey_BaseLayerFrameRateFraction: CFStringRef;
    pub static kVTCompressionPropertyKey_CalculateMeanSquaredError: CFStringRef;
    pub static kVTCompressionPropertyKey_CameraCalibrationDataLensCollection: CFStringRef;
    pub static kVTCompressionPropertyKey_CleanAperture: CFStringRef;
    pub static kVTCompressionPropertyKey_ConstantBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_ContentLightLevelInfo: CFStringRef;
    pub static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    pub static kVTCompressionPropertyKey_Depth: CFStringRef;
    pub static kVTCompressionPropertyKey_EnableLTR: CFStringRef;
    pub static kVTCompressionPropertyKey_EncoderID: CFStringRef;
    pub static kVTCompressionPropertyKey_EstimatedAverageBytesPerFrame: CFStringRef;
    pub static kVTCompressionPropertyKey_ExpectedDuration: CFStringRef;
    pub static kVTCompressionPropertyKey_FieldCount: CFStringRef;
    pub static kVTCompressionPropertyKey_FieldDetail: CFStringRef;
    pub static kVTCompressionPropertyKey_GammaLevel: CFStringRef;
    pub static kVTCompressionPropertyKey_HDRMetadataInsertionMode: CFStringRef;
    pub static kVTCompressionPropertyKey_HasLeftStereoEyeView: CFStringRef;
    pub static kVTCompressionPropertyKey_HasRightStereoEyeView: CFStringRef;
    pub static kVTCompressionPropertyKey_HeroEye: CFStringRef;
    pub static kVTCompressionPropertyKey_HorizontalDisparityAdjustment: CFStringRef;
    pub static kVTCompressionPropertyKey_HorizontalFieldOfView: CFStringRef;
    pub static kVTCompressionPropertyKey_MVHEVCLeftAndRightViewIDs: CFStringRef;
    pub static kVTCompressionPropertyKey_MVHEVCVideoLayerIDs: CFStringRef;
    pub static kVTCompressionPropertyKey_MVHEVCViewIDs: CFStringRef;
    pub static kVTCompressionPropertyKey_MasteringDisplayColorVolume: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxAllowedFrameQP: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxFrameDelayCount: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxH264SliceBytes: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
    pub static kVTCompressionPropertyKey_MaximizePowerEfficiency: CFStringRef;
    pub static kVTCompressionPropertyKey_MaximumRealTimeFrameRate: CFStringRef;
    pub static kVTCompressionPropertyKey_MinAllowedFrameQP: CFStringRef;
    pub static kVTCompressionPropertyKey_MoreFramesAfterEnd: CFStringRef;
    pub static kVTCompressionPropertyKey_MoreFramesBeforeStart: CFStringRef;
    pub static kVTCompressionPropertyKey_NumberOfPendingFrames: CFStringRef;
    pub static kVTCompressionPropertyKey_OutputBitDepth: CFStringRef;
    pub static kVTCompressionPropertyKey_PixelAspectRatio: CFStringRef;
    pub static kVTCompressionPropertyKey_PixelBufferPoolIsShared: CFStringRef;
    pub static kVTCompressionPropertyKey_PixelTransferProperties: CFStringRef;
    pub static kVTCompressionPropertyKey_PreserveAlphaChannel: CFStringRef;
    pub static kVTCompressionPropertyKey_PreserveDynamicHDRMetadata: CFStringRef;
    pub static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFStringRef;
    pub static kVTCompressionPropertyKey_ProgressiveScan: CFStringRef;
    pub static kVTCompressionPropertyKey_ProjectionKind: CFStringRef;
    pub static kVTCompressionPropertyKey_RecommendedParallelizationLimit: CFStringRef;
    pub static kVTCompressionPropertyKey_RecommendedParallelizedSubdivisionMinimumDuration:
        CFStringRef;
    pub static kVTCompressionPropertyKey_RecommendedParallelizedSubdivisionMinimumFrameCount:
        CFStringRef;
    pub static kVTCompressionPropertyKey_ReferenceBufferCount: CFStringRef;
    pub static kVTCompressionPropertyKey_SourceFrameCount: CFStringRef;
    pub static kVTCompressionPropertyKey_SpatialAdaptiveQPLevel: CFStringRef;
    pub static kVTCompressionPropertyKey_StereoCameraBaseline: CFStringRef;
    pub static kVTCompressionPropertyKey_SuggestedLookAheadFrameCount: CFStringRef;
    pub static kVTCompressionPropertyKey_SupportedPresetDictionaries: CFStringRef;
    pub static kVTCompressionPropertyKey_SupportsBaseFrameQP: CFStringRef;
    pub static kVTCompressionPropertyKey_TargetQualityForAlpha: CFStringRef;
    pub static kVTCompressionPropertyKey_UsingGPURegistryID: CFStringRef;
    pub static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTCompressionPropertyKey_VBVBufferDuration: CFStringRef;
    pub static kVTCompressionPropertyKey_VBVInitialDelayPercentage: CFStringRef;
    pub static kVTCompressionPropertyKey_VBVMaxBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_VariableBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_VideoEncoderPixelBufferAttributes: CFStringRef;
    pub static kVTCompressionPropertyKey_ViewPackingKind: CFStringRef;
    pub static kVTEncodeFrameOptionKey_AcknowledgedLTRTokens: CFStringRef;
    pub static kVTEncodeFrameOptionKey_BaseFrameQP: CFStringRef;
    pub static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    pub static kVTEncodeFrameOptionKey_ForceLTRRefresh: CFStringRef;
    pub static kVTHDRMetadataInsertionMode_Auto: CFStringRef;
    pub static kVTHDRMetadataInsertionMode_None: CFStringRef;
    pub static kVTHDRMetadataInsertionMode_RequestSDRRangePreservation: CFStringRef;
    pub static kVTHeroEye_Left: CFStringRef;
    pub static kVTHeroEye_Right: CFStringRef;
    pub static kVTProjectionKind_Equirectangular: CFStringRef;
    pub static kVTProjectionKind_HalfEquirectangular: CFStringRef;
    pub static kVTProjectionKind_ParametricImmersive: CFStringRef;
    pub static kVTProjectionKind_Rectilinear: CFStringRef;
    pub static kVTSampleAttachmentKey_QualityMetrics: CFStringRef;
    pub static kVTSampleAttachmentKey_RequireLTRAcknowledgementToken: CFStringRef;
    pub static kVTSampleAttachmentQualityMetricsKey_ChromaBlueMeanSquaredError: CFStringRef;
    pub static kVTSampleAttachmentQualityMetricsKey_ChromaRedMeanSquaredError: CFStringRef;
    pub static kVTSampleAttachmentQualityMetricsKey_LumaMeanSquaredError: CFStringRef;
    pub static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    pub static kVTVideoEncoderSpecification_PreferredEncoderGPURegistryID: CFStringRef;
    pub static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTVideoEncoderSpecification_RequiredEncoderGPURegistryID: CFStringRef;
    pub static kVTViewPackingKind_OverUnder: CFStringRef;
    pub static kVTViewPackingKind_SideBySide: CFStringRef;

    pub static kVTProfileLevel_H263_Profile0_Level10: CFStringRef;
    pub static kVTProfileLevel_H263_Profile0_Level45: CFStringRef;
    pub static kVTProfileLevel_H263_Profile3_Level45: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_1_3: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_3_0: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_3_1: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_3_2: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_4_0: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_4_1: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_4_2: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_5_0: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_5_1: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_5_2: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_H264_ConstrainedHigh_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_H264_Extended_5_0: CFStringRef;
    pub static kVTProfileLevel_H264_Extended_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_H264_High_3_0: CFStringRef;
    pub static kVTProfileLevel_H264_High_3_1: CFStringRef;
    pub static kVTProfileLevel_H264_High_3_2: CFStringRef;
    pub static kVTProfileLevel_H264_High_4_0: CFStringRef;
    pub static kVTProfileLevel_H264_High_4_1: CFStringRef;
    pub static kVTProfileLevel_H264_High_4_2: CFStringRef;
    pub static kVTProfileLevel_H264_High_5_0: CFStringRef;
    pub static kVTProfileLevel_H264_High_5_1: CFStringRef;
    pub static kVTProfileLevel_H264_High_5_2: CFStringRef;
    pub static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_H264_Main_3_0: CFStringRef;
    pub static kVTProfileLevel_H264_Main_3_1: CFStringRef;
    pub static kVTProfileLevel_H264_Main_3_2: CFStringRef;
    pub static kVTProfileLevel_H264_Main_4_0: CFStringRef;
    pub static kVTProfileLevel_H264_Main_4_1: CFStringRef;
    pub static kVTProfileLevel_H264_Main_4_2: CFStringRef;
    pub static kVTProfileLevel_H264_Main_5_0: CFStringRef;
    pub static kVTProfileLevel_H264_Main_5_1: CFStringRef;
    pub static kVTProfileLevel_H264_Main_5_2: CFStringRef;
    pub static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Main10_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Main42210_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Monochrome_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Monochrome10_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_MP4V_AdvancedSimple_L0: CFStringRef;
    pub static kVTProfileLevel_MP4V_AdvancedSimple_L1: CFStringRef;
    pub static kVTProfileLevel_MP4V_AdvancedSimple_L2: CFStringRef;
    pub static kVTProfileLevel_MP4V_AdvancedSimple_L3: CFStringRef;
    pub static kVTProfileLevel_MP4V_AdvancedSimple_L4: CFStringRef;
    pub static kVTProfileLevel_MP4V_Main_L2: CFStringRef;
    pub static kVTProfileLevel_MP4V_Main_L3: CFStringRef;
    pub static kVTProfileLevel_MP4V_Main_L4: CFStringRef;
    pub static kVTProfileLevel_MP4V_Simple_L0: CFStringRef;
    pub static kVTProfileLevel_MP4V_Simple_L1: CFStringRef;
    pub static kVTProfileLevel_MP4V_Simple_L2: CFStringRef;
    pub static kVTProfileLevel_MP4V_Simple_L3: CFStringRef;

    pub static kVTH264EntropyMode_CABAC: CFStringRef;
    pub static kVTH264EntropyMode_CAVLC: CFStringRef;

    // ---- VTDecompressionSession (decoder side, added v0.3) ----
    pub fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;

    pub fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    pub fn VTDecompressionSessionGetTypeID() -> usize;

    pub fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: VTDecodeFrameFlags,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut VTDecodeInfoFlags,
    ) -> OSStatus;
    pub fn VTDecompressionSessionDecodeFrameWithOutputHandler(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: VTDecodeFrameFlags,
        info_flags_out: *mut VTDecodeInfoFlags,
        output_handler: *const c_void,
    ) -> OSStatus;
    pub fn VTDecompressionSessionDecodeFrameWithMultiImageCapableOutputHandler(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: VTDecodeFrameFlags,
        info_flags_out: *mut VTDecodeInfoFlags,
        multi_image_capable_output_handler: *const c_void,
    ) -> OSStatus;
    // Arcen patch: `VTDecompressionSessionDecodeFrameWithOptions` and
    // `...WithOptionsAndOutputHandler` are deliberately not linked. They are
    // newer than macOS 12, so a strong reference stops the binary loading
    // there. `DecompressionSession::decode_with_options` resolves the options
    // variant at runtime instead; see `decompression/mod.rs`.

    pub fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;

    pub fn VTDecompressionSessionFinishDelayedFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;

    pub fn VTDecompressionSessionCanAcceptFormatDescription(
        session: VTDecompressionSessionRef,
        new_format_desc: CMFormatDescriptionRef,
    ) -> bool;
    pub fn VTDecompressionSessionCopyBlackPixelBuffer(
        session: VTDecompressionSessionRef,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> OSStatus;
    pub fn VTIsHardwareDecodeSupported(codec_type: CMVideoCodecType) -> Boolean;
    pub fn VTIsStereoMVHEVCDecodeSupported() -> Boolean;
    pub fn VTDecompressionSessionSetMultiImageCallback(
        decompression_session: VTDecompressionSessionRef,
        output_multi_image_callback: VTDecompressionOutputMultiImageCallback,
        output_multi_image_ref_con: *mut c_void,
    ) -> OSStatus;

    pub static kVTDecompressionPropertyKey_RealTime: CFStringRef;
    pub static kVTDecompressionPropertyKey_MaximumOutputBufferDepth: CFStringRef;
    pub static kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder: CFStringRef;
    pub static kVTDecodeFrameOptionKey_ContentAnalyzerCropRectangle: CFStringRef;
    pub static kVTDecodeFrameOptionKey_ContentAnalyzerRotation: CFStringRef;
    pub static kVTDecompressionPropertyKey_AllowBitstreamToChangeFrameDimensions: CFStringRef;
    pub static kVTDecompressionPropertyKey_ContentHasInterframeDependencies: CFStringRef;
    pub static kVTDecompressionPropertyKey_DecoderProducesRAWOutput: CFStringRef;
    pub static kVTDecompressionPropertyKey_DeinterlaceMode: CFStringRef;
    pub static kVTDecompressionPropertyKey_FieldMode: CFStringRef;
    pub static kVTDecompressionPropertyKey_GeneratePerFrameHDRDisplayMetadata: CFStringRef;
    pub static kVTDecompressionPropertyKey_MaxOutputPresentationTimeStampOfFramesBeingDecoded:
        CFStringRef;
    pub static kVTDecompressionPropertyKey_MaximizePowerEfficiency: CFStringRef;
    pub static kVTDecompressionPropertyKey_MinOutputPresentationTimeStampOfFramesBeingDecoded:
        CFStringRef;
    pub static kVTDecompressionPropertyKey_NumberOfFramesBeingDecoded: CFStringRef;
    pub static kVTDecompressionPropertyKey_OnlyTheseFrames: CFStringRef;
    pub static kVTDecompressionPropertyKey_OutputPoolRequestedMinimumBufferCount: CFStringRef;
    pub static kVTDecompressionPropertyKey_PixelBufferPool: CFStringRef;
    pub static kVTDecompressionPropertyKey_PixelBufferPoolIsShared: CFStringRef;
    pub static kVTDecompressionPropertyKey_PixelFormatsWithReducedResolutionSupport: CFStringRef;
    pub static kVTDecompressionPropertyKey_PixelTransferProperties: CFStringRef;
    pub static kVTDecompressionPropertyKey_PropagatePerFrameHDRDisplayMetadata: CFStringRef;
    pub static kVTDecompressionPropertyKey_ReducedCoefficientDecode: CFStringRef;
    pub static kVTDecompressionPropertyKey_ReducedFrameDelivery: CFStringRef;
    pub static kVTDecompressionPropertyKey_ReducedResolutionDecode: CFStringRef;
    pub static kVTDecompressionPropertyKey_RequestRAWOutput: CFStringRef;
    pub static kVTDecompressionPropertyKey_RequestedMVHEVCVideoLayerIDs: CFStringRef;
    pub static kVTDecompressionPropertyKey_SuggestedQualityOfServiceTiers: CFStringRef;
    pub static kVTDecompressionPropertyKey_SupportedPixelFormatsOrderedByPerformance: CFStringRef;
    pub static kVTDecompressionPropertyKey_SupportedPixelFormatsOrderedByQuality: CFStringRef;
    pub static kVTDecompressionPropertyKey_ThreadCount: CFStringRef;
    pub static kVTDecompressionPropertyKey_UsingGPURegistryID: CFStringRef;
    pub static kVTDecompressionProperty_DeinterlaceMode_Temporal: CFStringRef;
    pub static kVTDecompressionProperty_DeinterlaceMode_VerticalFilter: CFStringRef;
    pub static kVTDecompressionProperty_FieldMode_BothFields: CFStringRef;
    pub static kVTDecompressionProperty_FieldMode_BottomFieldOnly: CFStringRef;
    pub static kVTDecompressionProperty_FieldMode_DeinterlaceFields: CFStringRef;
    pub static kVTDecompressionProperty_FieldMode_SingleField: CFStringRef;
    pub static kVTDecompressionProperty_FieldMode_TopFieldOnly: CFStringRef;
    pub static kVTDecompressionProperty_OnlyTheseFrames_AllFrames: CFStringRef;
    pub static kVTDecompressionProperty_OnlyTheseFrames_IFrames: CFStringRef;
    pub static kVTDecompressionProperty_OnlyTheseFrames_KeyFrames: CFStringRef;
    pub static kVTDecompressionProperty_OnlyTheseFrames_NonDroppableFrames: CFStringRef;
    pub static kVTDecompressionProperty_TemporalLevelLimit: CFStringRef;
    pub static kVTDecompressionResolutionKey_Height: CFStringRef;
    pub static kVTDecompressionResolutionKey_Width: CFStringRef;
    pub static kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: CFStringRef;
    pub static kVTVideoDecoderSpecification_PreferredDecoderGPURegistryID: CFStringRef;
    pub static kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: CFStringRef;
    pub static kVTVideoDecoderSpecification_RequiredDecoderGPURegistryID: CFStringRef;

    // ---- VTPixelTransferSession (v0.6) ----
    pub fn VTPixelTransferSessionCreate(
        allocator: CFAllocatorRef,
        pixel_transfer_session_out: *mut VTPixelTransferSessionRef,
    ) -> OSStatus;
    pub fn VTPixelTransferSessionInvalidate(session: VTPixelTransferSessionRef);
    pub fn VTPixelTransferSessionGetTypeID() -> usize;
    pub fn VTPixelTransferSessionTransferImage(
        session: VTPixelTransferSessionRef,
        source_buffer: CVPixelBufferRef,
        destination_buffer: CVPixelBufferRef,
    ) -> OSStatus;
    pub static kVTPixelTransferPropertyKey_ScalingMode: CFStringRef;
    pub static kVTScalingMode_Normal: CFStringRef;
    pub static kVTScalingMode_CropSourceToCleanAperture: CFStringRef;
    pub static kVTScalingMode_Letterbox: CFStringRef;
    pub static kVTScalingMode_Trim: CFStringRef;
    pub static kVTPixelTransferPropertyKey_DestinationCleanAperture: CFStringRef;
    pub static kVTPixelTransferPropertyKey_DestinationPixelAspectRatio: CFStringRef;
    pub static kVTPixelTransferPropertyKey_DownsamplingMode: CFStringRef;
    pub static kVTDownsamplingMode_Decimate: CFStringRef;
    pub static kVTDownsamplingMode_Average: CFStringRef;
    pub static kVTPixelTransferPropertyKey_DestinationColorPrimaries: CFStringRef;
    pub static kVTPixelTransferPropertyKey_DestinationTransferFunction: CFStringRef;
    pub static kVTPixelTransferPropertyKey_DestinationICCProfile: CFStringRef;
    pub static kVTPixelTransferPropertyKey_DestinationYCbCrMatrix: CFStringRef;
    pub static kVTPixelTransferPropertyKey_RealTime: CFStringRef;

    // ---- VTPixelRotationSession (v0.6) ----
    pub fn VTPixelRotationSessionCreate(
        allocator: CFAllocatorRef,
        pixel_rotation_session_out: *mut VTPixelRotationSessionRef,
    ) -> OSStatus;
    pub fn VTPixelRotationSessionInvalidate(session: VTPixelRotationSessionRef);
    pub fn VTPixelRotationSessionGetTypeID() -> usize;
    pub fn VTPixelRotationSessionRotateImage(
        session: VTPixelRotationSessionRef,
        source_buffer: CVPixelBufferRef,
        destination_buffer: CVPixelBufferRef,
    ) -> OSStatus;

    pub static kVTPixelRotationPropertyKey_Rotation: CFStringRef;
    pub static kVTPixelRotationPropertyKey_FlipHorizontalOrientation: CFStringRef;
    pub static kVTPixelRotationPropertyKey_FlipVerticalOrientation: CFStringRef;

    pub static kVTRotation_0: CFStringRef;
    pub static kVTRotation_CW90: CFStringRef;
    pub static kVTRotation_180: CFStringRef;
    pub static kVTRotation_CCW90: CFStringRef;

    // ---- VTVideoEncoderList (v0.7) ----
    pub fn VTCopyVideoEncoderList(options: CFDictionaryRef, list_out: *mut CFArrayRef) -> OSStatus;
    pub fn VTCopySupportedPropertyDictionaryForEncoder(
        width: i32,
        height: i32,
        codec_type: CMVideoCodecType,
        encoder_specification: CFDictionaryRef,
        encoder_id_out: *mut CFStringRef,
        supported_properties_out: *mut CFDictionaryRef,
    ) -> OSStatus;
    pub static kVTVideoEncoderListOption_IncludeStandardDefinitionDVEncoders: CFStringRef;
    pub static kVTVideoEncoderList_CodecType: CFStringRef;
    pub static kVTVideoEncoderList_EncoderID: CFStringRef;
    pub static kVTVideoEncoderList_CodecName: CFStringRef;
    pub static kVTVideoEncoderList_EncoderName: CFStringRef;
    pub static kVTVideoEncoderList_DisplayName: CFStringRef;
    pub static kVTVideoEncoderList_GPURegistryID: CFStringRef;
    pub static kVTVideoEncoderList_InstanceLimit: CFStringRef;
    pub static kVTVideoEncoderList_IsHardwareAccelerated: CFStringRef;
    pub static kVTVideoEncoderList_PerformanceRating: CFStringRef;
    pub static kVTVideoEncoderList_QualityRating: CFStringRef;
    pub static kVTVideoEncoderList_SupportedSelectionProperties: CFStringRef;
    pub static kVTVideoEncoderList_SupportsFrameReordering: CFStringRef;

    // ---- VTFrameSilo (v0.9) ----
    pub fn VTFrameSiloGetTypeID() -> usize;
    pub fn VTFrameSiloCreate(
        allocator: CFAllocatorRef,
        file_url: *const c_void,
        time_range: CMTimeRange,
        options: CFDictionaryRef,
        silo_out: *mut VTFrameSiloRef,
    ) -> OSStatus;
    pub fn VTFrameSiloAddSampleBuffer(
        silo: VTFrameSiloRef,
        sample_buffer: CMSampleBufferRef,
    ) -> OSStatus;
    pub fn VTFrameSiloGetProgressOfCurrentPass(
        silo: VTFrameSiloRef,
        progress_out: *mut f32,
    ) -> OSStatus;
    pub fn VTFrameSiloSetTimeRangesForNextPass(
        silo: VTFrameSiloRef,
        time_range_count: CMItemCount,
        time_range_array: *const CMTimeRange,
    ) -> OSStatus;
    pub fn VTFrameSiloCallFunctionForEachSampleBuffer(
        silo: VTFrameSiloRef,
        time_range: CMTimeRange,
        refcon: *mut c_void,
        callback: Option<unsafe extern "C" fn(*mut c_void, CMSampleBufferRef) -> OSStatus>,
    ) -> OSStatus;
    pub fn VTFrameSiloCallBlockForEachSampleBuffer(
        silo: VTFrameSiloRef,
        time_range: CMTimeRange,
        handler: *const c_void,
    ) -> OSStatus;

    // ---- VTMultiPassStorage (v0.9) ----
    pub fn VTMultiPassStorageGetTypeID() -> usize;
    pub fn VTMultiPassStorageCreate(
        allocator: CFAllocatorRef,
        file_url: *const c_void,
        time_range: CMTimeRange,
        options: CFDictionaryRef,
        storage_out: *mut VTMultiPassStorageRef,
    ) -> OSStatus;
    pub static kVTMultiPassStorageCreationOption_DoNotDelete: CFStringRef;
    pub fn VTMultiPassStorageClose(storage: VTMultiPassStorageRef) -> OSStatus;

    // ---- VTHDRPerFrameMetadataGenerationSession (v0.9) ----
    pub fn VTHDRPerFrameMetadataGenerationSessionGetTypeID() -> usize;
    pub fn VTHDRPerFrameMetadataGenerationSessionCreate(
        allocator: CFAllocatorRef,
        frames_per_second: f32,
        options: CFDictionaryRef,
        session_out: *mut VTHDRPerFrameMetadataGenerationSessionRef,
    ) -> OSStatus;
    pub fn VTHDRPerFrameMetadataGenerationSessionAttachMetadata(
        session: VTHDRPerFrameMetadataGenerationSessionRef,
        pixel_buffer: CVPixelBufferRef,
        scene_change: bool,
    ) -> OSStatus;
    pub static kVTHDRPerFrameMetadataGenerationHDRFormatType_DolbyVision:
        VTHDRPerFrameMetadataGenerationHDRFormatType;
    pub static kVTHDRPerFrameMetadataGenerationOptionsKey_HDRFormats: CFStringRef;

    // ---- VTUtilities + VTProfessionalVideoWorkflow (v0.9) ----
    pub fn VTCreateCGImageFromCVPixelBuffer(
        pixel_buffer: CVPixelBufferRef,
        options: CFDictionaryRef,
        image_out: *mut *mut c_void,
    ) -> OSStatus;
    pub fn VTRegisterSupplementalVideoDecoderIfAvailable(codec_type: CMVideoCodecType);
    pub fn VTCopyVideoDecoderExtensionProperties(
        format_description: CMFormatDescriptionRef,
        media_extension_properties_out: *mut CFDictionaryRef,
    ) -> OSStatus;
    pub fn VTCopyRAWProcessorExtensionProperties(
        format_description: CMFormatDescriptionRef,
        media_extension_properties_out: *mut CFDictionaryRef,
    ) -> OSStatus;
    pub static kVTExtensionProperties_CodecNameKey: VTExtensionPropertiesKey;
    pub static kVTExtensionProperties_ContainingBundleNameKey: VTExtensionPropertiesKey;
    pub static kVTExtensionProperties_ContainingBundleURLKey: VTExtensionPropertiesKey;
    pub static kVTExtensionProperties_ExtensionIdentifierKey: VTExtensionPropertiesKey;
    pub static kVTExtensionProperties_ExtensionNameKey: VTExtensionPropertiesKey;
    pub static kVTExtensionProperties_ExtensionURLKey: VTExtensionPropertiesKey;
    pub fn VTRegisterProfessionalVideoWorkflowVideoDecoders();
    pub fn VTRegisterProfessionalVideoWorkflowVideoEncoders();

    // ---- VTMotionEstimationSession (v0.10, macOS 26+) ----
    pub fn VTMotionEstimationSessionGetTypeID() -> usize;
    pub fn VTMotionEstimationSessionCreate(
        allocator: CFAllocatorRef,
        selection_options: CFDictionaryRef,
        width: u32,
        height: u32,
        session_out: *mut VTMotionEstimationSessionRef,
    ) -> OSStatus;
    pub fn VTMotionEstimationSessionInvalidate(session: VTMotionEstimationSessionRef);
    pub fn VTMotionEstimationSessionCopySourcePixelBufferAttributes(
        session: VTMotionEstimationSessionRef,
        attributes_out: *mut CFDictionaryRef,
    ) -> OSStatus;
    pub fn VTMotionEstimationSessionCompleteFrames(
        session: VTMotionEstimationSessionRef,
    ) -> OSStatus;
    pub static kVTMotionEstimationSessionCreationOption_Label: CFStringRef;
    pub static kVTMotionEstimationSessionCreationOption_MotionVectorSize: CFStringRef;
    pub static kVTMotionEstimationSessionCreationOption_UseMultiPassSearch: CFStringRef;

    // ---- VTRAWProcessingSession (v0.10, macOS 15+) ----
    pub fn VTRAWProcessingSessionGetTypeID() -> usize;
    pub fn VTRAWProcessingSessionCreate(
        allocator: CFAllocatorRef,
        format_description: *const c_void,
        output_pixel_buffer_attributes: CFDictionaryRef,
        processing_session_options: CFDictionaryRef,
        session_out: *mut VTRAWProcessingSessionRef,
    ) -> OSStatus;
    pub fn VTRAWProcessingSessionInvalidate(session: VTRAWProcessingSessionRef);
    pub fn VTRAWProcessingSessionCompleteFrames(session: VTRAWProcessingSessionRef) -> OSStatus;
    pub fn VTRAWProcessingSessionCopyProcessingParameters(
        session: VTRAWProcessingSessionRef,
        out_parameter_array: *mut CFArrayRef,
    ) -> OSStatus;
    pub fn VTRAWProcessingSessionSetProcessingParameters(
        session: VTRAWProcessingSessionRef,
        processing_parameters: CFDictionaryRef,
    ) -> OSStatus;
    pub fn VTRAWProcessingSessionSetParameterChangedHandler(
        session: VTRAWProcessingSessionRef,
        parameter_change_handler: *const c_void,
    ) -> OSStatus;
    pub static kVTRAWProcessingPropertyKey_MetadataForSidecarFile: CFStringRef;
    pub static kVTRAWProcessingPropertyKey_MetalDeviceRegistryID: CFStringRef;
    pub static kVTRAWProcessingPropertyKey_OutputColorAttachments: CFStringRef;

    // ---- VTRAW parameter keys (v0.10) ----
    pub static kVTRAWProcessingParameter_Key: CFStringRef;
    pub static kVTRAWProcessingParameter_Name: CFStringRef;
    pub static kVTRAWProcessingParameter_Description: CFStringRef;
    pub static kVTRAWProcessingParameter_Enabled: CFStringRef;
    pub static kVTRAWProcessingParameter_ValueType: CFStringRef;
    pub static kVTRAWProcessingParameterValueType_Boolean: CFStringRef;
    pub static kVTRAWProcessingParameterValueType_Integer: CFStringRef;
    pub static kVTRAWProcessingParameterValueType_Float: CFStringRef;
    pub static kVTRAWProcessingParameterValueType_List: CFStringRef;
    pub static kVTRAWProcessingParameterValueType_SubGroup: CFStringRef;
    pub static kVTRAWProcessingParameter_ListArray: CFStringRef;
    pub static kVTRAWProcessingParameterListElement_Label: CFStringRef;
    pub static kVTRAWProcessingParameterListElement_Description: CFStringRef;
    pub static kVTRAWProcessingParameterListElement_ListElementID: CFStringRef;
    pub static kVTRAWProcessingParameter_SubGroup: CFStringRef;
    pub static kVTRAWProcessingParameter_MaximumValue: CFStringRef;
    pub static kVTRAWProcessingParameter_MinimumValue: CFStringRef;
    pub static kVTRAWProcessingParameter_InitialValue: CFStringRef;
    pub static kVTRAWProcessingParameter_NeutralValue: CFStringRef;
    pub static kVTRAWProcessingParameter_CameraValue: CFStringRef;
    pub static kVTRAWProcessingParameter_CurrentValue: CFStringRef;
}

pub type VTFrameSiloRef = *mut c_void;
pub type VTMultiPassStorageRef = *mut c_void;
pub type VTHDRPerFrameMetadataGenerationSessionRef = *mut c_void;
pub type VTMotionEstimationSessionRef = *mut c_void;
pub type VTRAWProcessingSessionRef = *mut c_void;

pub type VTPixelTransferSessionRef = *mut c_void;
pub type VTPixelRotationSessionRef = *mut c_void;

pub type VTDecompressionSessionRef = *mut c_void;
pub type VTDecodeFrameFlags = u32;
pub type VTDecodeInfoFlags = u32;
pub type VTDecompressionOutputCallback = unsafe extern "C" fn(
    decompression_output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: VTDecodeInfoFlags,
    image_buffer: *mut c_void,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);
pub type VTDecompressionOutputMultiImageCallback = unsafe extern "C" fn(
    decompression_output_multi_image_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: VTDecodeInfoFlags,
    tagged_buffer_group: CMTaggedBufferGroupRef,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

#[repr(C)]
pub struct VTDecompressionOutputCallbackRecord {
    pub decompression_output_callback: VTDecompressionOutputCallback,
    pub decompression_output_ref_con: *mut c_void,
}

// ---- ABI layout assertions ----
//
// `VTDecompressionOutputCallbackRecord` is passed by value into
// `VTDecompressionSessionCreate`. Its layout must match VideoToolbox's
// C `VTDecompressionOutputCallbackRecord` exactly: a function pointer
// followed by a `void *`, i.e. two pointer-sized fields.
//
// The crate MSRV (1.76) predates stable `offset_of!` (1.77), so field
// offsets are pinned indirectly via `size_of`/`align_of` rather than
// `offset_of!`. These compile-time asserts catch accidental field
// reordering or type changes at build time; `vt_verify_ffi_layout` and
// `tests/ffi_layout_tests.rs` guard the same invariant at run time.
const _: () = assert!(
    core::mem::size_of::<VTDecompressionOutputCallbackRecord>()
        == 2 * core::mem::size_of::<*mut c_void>()
);
const _: () = assert!(
    core::mem::align_of::<VTDecompressionOutputCallbackRecord>()
        == core::mem::align_of::<*mut c_void>()
);

/// Verify, at run time, that the `#[repr(C)]` FFI structs shared with the
/// C `VideoToolbox` ABI have the size and alignment the bindings expect.
///
/// Returns `true` when every checked struct matches its pinned layout. A
/// `false` return means the Rust binding layout has drifted from the C ABI,
/// which is a real ABI bug. Verified by `tests/ffi_layout_tests.rs`.
#[must_use]
pub const fn vt_verify_ffi_layout() -> bool {
    let ptr = core::mem::size_of::<*mut c_void>();
    let ptr_align = core::mem::align_of::<*mut c_void>();

    core::mem::size_of::<VTDecompressionOutputCallbackRecord>() == 2 * ptr
        && core::mem::align_of::<VTDecompressionOutputCallbackRecord>() == ptr_align
}

// Suppress unused variant warnings on c_uint placeholder types.
const _: () = {
    let _ = core::mem::size_of::<c_uint>();
};
