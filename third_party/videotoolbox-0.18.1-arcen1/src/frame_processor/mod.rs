#![allow(
    clippy::cast_possible_wrap,
    clippy::missing_const_for_fn,
    clippy::missing_errors_doc,
    clippy::too_many_arguments
)]

//! `VTFrameProcessor` capability queries plus retained wrappers for
//! `VTFrameProcessorFrame` / `VTFrameProcessorOpticalFlow` and
//! synchronous processing helpers for all public pipelines this crate
//! exposes.

use core::ffi::c_void;

use apple_cf::{cm::CMTime, cv::CVPixelBuffer};
use apple_metal::CommandBuffer;

use crate::{ffi, VTError};

extern "C" {
    fn vt_super_resolution_is_supported() -> bool;
    fn vt_motion_blur_is_supported() -> bool;
    fn vt_temporal_noise_filter_is_supported() -> bool;
    fn vt_frame_rate_conversion_is_supported() -> bool;
    fn vt_low_latency_super_resolution_is_supported() -> bool;
    fn vt_low_latency_frame_interpolation_is_supported() -> bool;
    fn vt_optical_flow_is_supported() -> bool;

    fn vt_super_resolution_supported_scale_factors(out_buf: *mut u32, max: usize) -> usize;
    fn vt_low_latency_super_resolution_supported_scale_factors(
        frame_width: isize,
        frame_height: isize,
        out_buf: *mut f32,
        max: usize,
    ) -> usize;

    fn vt_super_resolution_model_status(
        frame_width: isize,
        frame_height: isize,
        scale_factor: isize,
        use_precomputed_flow: bool,
        input_type: isize,
        quality_prioritization: isize,
        revision: isize,
    ) -> i32;
    fn vt_super_resolution_model_percentage_available(
        frame_width: isize,
        frame_height: isize,
        scale_factor: isize,
        use_precomputed_flow: bool,
        input_type: isize,
        quality_prioritization: isize,
        revision: isize,
    ) -> f32;
    fn vt_super_resolution_download_model(
        frame_width: isize,
        frame_height: isize,
        scale_factor: isize,
        use_precomputed_flow: bool,
        input_type: isize,
        quality_prioritization: isize,
        revision: isize,
    ) -> i32;

    fn vt_super_resolution_start(
        frame_width: isize,
        frame_height: isize,
        scale_factor: isize,
        use_precomputed_flow: bool,
        input_type: isize,
        quality_prioritization: isize,
        revision: isize,
        out: *mut *mut c_void,
    ) -> i32;
    fn vt_motion_blur_start(
        frame_width: isize,
        frame_height: isize,
        use_precomputed_flow: bool,
        quality_prioritization: isize,
        revision: isize,
        out: *mut *mut c_void,
    ) -> i32;
    fn vt_temporal_noise_filter_start(
        frame_width: isize,
        frame_height: isize,
        source_pixel_format: u32,
        out: *mut *mut c_void,
    ) -> i32;
    fn vt_frame_rate_conversion_start(
        frame_width: isize,
        frame_height: isize,
        use_precomputed_flow: bool,
        quality_prioritization: isize,
        revision: isize,
        out: *mut *mut c_void,
    ) -> i32;
    fn vt_low_latency_super_resolution_start(
        frame_width: isize,
        frame_height: isize,
        scale_factor: f32,
        out: *mut *mut c_void,
    ) -> i32;
    fn vt_low_latency_frame_interpolation_start(
        frame_width: isize,
        frame_height: isize,
        number_of_interpolated_frames: isize,
        out: *mut *mut c_void,
    ) -> i32;
    fn vt_optical_flow_start(
        frame_width: isize,
        frame_height: isize,
        quality_prioritization: isize,
        revision: isize,
        out: *mut *mut c_void,
    ) -> i32;

    fn vt_frame_processor_frame_create(
        buffer: *mut c_void,
        presentation_time_stamp: ffi::CMTime,
        out: *mut *mut c_void,
    ) -> i32;
    fn vt_frame_processor_frame_release(frame: *mut c_void);

    fn vt_frame_processor_optical_flow_create(
        forward_flow: *mut c_void,
        backward_flow: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn vt_frame_processor_optical_flow_release(flow: *mut c_void);

    fn vt_frame_processor_process_super_resolution(
        processor: *mut c_void,
        source_frame: *mut c_void,
        previous_frame: *mut c_void,
        previous_output_frame: *mut c_void,
        optical_flow: *mut c_void,
        submission_mode: i32,
        destination_frame: *mut c_void,
    ) -> i32;
    fn vt_frame_processor_process_super_resolution_with_command_buffer(
        processor: *mut c_void,
        command_buffer: *mut c_void,
        source_frame: *mut c_void,
        previous_frame: *mut c_void,
        previous_output_frame: *mut c_void,
        optical_flow: *mut c_void,
        submission_mode: i32,
        destination_frame: *mut c_void,
    ) -> i32;
    fn vt_frame_processor_process_motion_blur(
        processor: *mut c_void,
        source_frame: *mut c_void,
        next_frame: *mut c_void,
        previous_frame: *mut c_void,
        next_optical_flow: *mut c_void,
        previous_optical_flow: *mut c_void,
        motion_blur_strength: isize,
        submission_mode: i32,
        destination_frame: *mut c_void,
    ) -> i32;
    fn vt_frame_processor_process_motion_blur_with_command_buffer(
        processor: *mut c_void,
        command_buffer: *mut c_void,
        source_frame: *mut c_void,
        next_frame: *mut c_void,
        previous_frame: *mut c_void,
        next_optical_flow: *mut c_void,
        previous_optical_flow: *mut c_void,
        motion_blur_strength: isize,
        submission_mode: i32,
        destination_frame: *mut c_void,
    ) -> i32;
    fn vt_frame_processor_process_temporal_noise_filter(
        processor: *mut c_void,
        source_frame: *mut c_void,
        next_frames: *const *mut c_void,
        next_frame_count: usize,
        previous_frames: *const *mut c_void,
        previous_frame_count: usize,
        destination_frame: *mut c_void,
        filter_strength: f32,
        has_discontinuity: bool,
    ) -> i32;
    fn vt_frame_processor_process_temporal_noise_filter_with_command_buffer(
        processor: *mut c_void,
        command_buffer: *mut c_void,
        source_frame: *mut c_void,
        next_frames: *const *mut c_void,
        next_frame_count: usize,
        previous_frames: *const *mut c_void,
        previous_frame_count: usize,
        destination_frame: *mut c_void,
        filter_strength: f32,
        has_discontinuity: bool,
    ) -> i32;
    fn vt_frame_processor_process_frame_rate_conversion(
        processor: *mut c_void,
        source_frame: *mut c_void,
        next_frame: *mut c_void,
        optical_flow: *mut c_void,
        interpolation_phase: *const f32,
        interpolation_phase_count: usize,
        submission_mode: i32,
        destination_frames: *const *mut c_void,
        destination_frame_count: usize,
    ) -> i32;
    fn vt_frame_processor_process_frame_rate_conversion_with_command_buffer(
        processor: *mut c_void,
        command_buffer: *mut c_void,
        source_frame: *mut c_void,
        next_frame: *mut c_void,
        optical_flow: *mut c_void,
        interpolation_phase: *const f32,
        interpolation_phase_count: usize,
        submission_mode: i32,
        destination_frames: *const *mut c_void,
        destination_frame_count: usize,
    ) -> i32;
    fn vt_frame_processor_process_low_latency_super_resolution(
        processor: *mut c_void,
        source_frame: *mut c_void,
        destination_frame: *mut c_void,
    ) -> i32;
    fn vt_frame_processor_process_low_latency_super_resolution_with_command_buffer(
        processor: *mut c_void,
        command_buffer: *mut c_void,
        source_frame: *mut c_void,
        destination_frame: *mut c_void,
    ) -> i32;
    fn vt_frame_processor_process_low_latency_frame_interpolation(
        processor: *mut c_void,
        source_frame: *mut c_void,
        previous_frame: *mut c_void,
        interpolation_phase: *const f32,
        interpolation_phase_count: usize,
        destination_frames: *const *mut c_void,
        destination_frame_count: usize,
    ) -> i32;
    fn vt_frame_processor_process_low_latency_frame_interpolation_with_command_buffer(
        processor: *mut c_void,
        command_buffer: *mut c_void,
        source_frame: *mut c_void,
        previous_frame: *mut c_void,
        interpolation_phase: *const f32,
        interpolation_phase_count: usize,
        destination_frames: *const *mut c_void,
        destination_frame_count: usize,
    ) -> i32;
    fn vt_frame_processor_process_optical_flow(
        processor: *mut c_void,
        source_frame: *mut c_void,
        next_frame: *mut c_void,
        submission_mode: i32,
        destination_optical_flow: *mut c_void,
    ) -> i32;
    fn vt_frame_processor_process_optical_flow_with_command_buffer(
        processor: *mut c_void,
        command_buffer: *mut c_void,
        source_frame: *mut c_void,
        next_frame: *mut c_void,
        submission_mode: i32,
        destination_optical_flow: *mut c_void,
    ) -> i32;

    fn vt_frame_processor_end(processor: *mut c_void);
    fn vt_frame_processor_release(processor: *mut c_void);
}

/// Per-effect availability snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct FrameProcessorCapabilities {
    pub super_resolution: bool,
    pub motion_blur: bool,
    pub temporal_noise_filter: bool,
    pub frame_rate_conversion: bool,
    pub low_latency_super_resolution: bool,
    pub low_latency_frame_interpolation: bool,
    pub optical_flow: bool,
}

/// Submission ordering hint for processors that accept random vs
/// sequential frame order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum FrameProcessorSubmissionMode {
    Random = 1,
    Sequential = 2,
}

/// Submission ordering hint for frame-rate conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum FrameRateConversionSubmissionMode {
    Random = 1,
    Sequential = 2,
    SequentialReferencesUnchanged = 3,
}

/// `VTFrameRateConversionConfiguration.QualityPrioritization`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTFrameRateConversionConfigurationQualityPrioritization {
    Normal = ffi::VTFrameRateConversionConfigurationQualityPrioritizationNormal,
    Quality = ffi::VTFrameRateConversionConfigurationQualityPrioritizationQuality,
}

/// `VTFrameRateConversionConfiguration.Revision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTFrameRateConversionConfigurationRevision {
    Revision1 = ffi::VTFrameRateConversionConfigurationRevision1,
}

/// `VTMotionBlurConfiguration.QualityPrioritization`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTMotionBlurConfigurationQualityPrioritization {
    Normal = ffi::VTMotionBlurConfigurationQualityPrioritizationNormal,
    Quality = ffi::VTMotionBlurConfigurationQualityPrioritizationQuality,
}

/// `VTMotionBlurConfiguration.Revision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTMotionBlurConfigurationRevision {
    Revision1 = ffi::VTMotionBlurConfigurationRevision1,
}

/// `VTOpticalFlowConfiguration.QualityPrioritization`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTOpticalFlowConfigurationQualityPrioritization {
    Normal = ffi::VTOpticalFlowConfigurationQualityPrioritizationNormal,
    Quality = ffi::VTOpticalFlowConfigurationQualityPrioritizationQuality,
}

/// `VTOpticalFlowConfiguration.Revision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTOpticalFlowConfigurationRevision {
    Revision1 = ffi::VTOpticalFlowConfigurationRevision1,
}

/// `VTSuperResolutionScalerConfiguration.InputType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTSuperResolutionScalerConfigurationInputType {
    Video = ffi::VTSuperResolutionScalerConfigurationInputTypeVideo,
    Image = ffi::VTSuperResolutionScalerConfigurationInputTypeImage,
}

/// `VTSuperResolutionScalerConfiguration.QualityPrioritization`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTSuperResolutionScalerConfigurationQualityPrioritization {
    Normal = ffi::VTSuperResolutionScalerConfigurationQualityPrioritizationNormal,
}

/// `VTSuperResolutionScalerConfiguration.Revision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum VTSuperResolutionScalerConfigurationRevision {
    Revision1 = ffi::VTSuperResolutionScalerConfigurationRevision1,
}

/// Explicit configuration for `VTSuperResolutionScalerConfiguration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuperResolutionConfiguration {
    pub frame_width: usize,
    pub frame_height: usize,
    pub scale_factor: usize,
    pub use_precomputed_flow: bool,
    pub input_type: VTSuperResolutionScalerConfigurationInputType,
    pub quality_prioritization: VTSuperResolutionScalerConfigurationQualityPrioritization,
    pub revision: VTSuperResolutionScalerConfigurationRevision,
}

impl SuperResolutionConfiguration {
    #[must_use]
    pub const fn new(frame_width: usize, frame_height: usize, scale_factor: usize) -> Self {
        Self {
            frame_width,
            frame_height,
            scale_factor,
            use_precomputed_flow: false,
            input_type: VTSuperResolutionScalerConfigurationInputType::Video,
            quality_prioritization:
                VTSuperResolutionScalerConfigurationQualityPrioritization::Normal,
            revision: VTSuperResolutionScalerConfigurationRevision::Revision1,
        }
    }
}

/// Explicit configuration for `VTMotionBlurConfiguration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionBlurConfiguration {
    pub frame_width: usize,
    pub frame_height: usize,
    pub use_precomputed_flow: bool,
    pub quality_prioritization: VTMotionBlurConfigurationQualityPrioritization,
    pub revision: VTMotionBlurConfigurationRevision,
}

impl MotionBlurConfiguration {
    #[must_use]
    pub const fn new(frame_width: usize, frame_height: usize) -> Self {
        Self {
            frame_width,
            frame_height,
            use_precomputed_flow: false,
            quality_prioritization: VTMotionBlurConfigurationQualityPrioritization::Normal,
            revision: VTMotionBlurConfigurationRevision::Revision1,
        }
    }
}

/// Explicit configuration for `VTFrameRateConversionConfiguration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRateConversionConfiguration {
    pub frame_width: usize,
    pub frame_height: usize,
    pub use_precomputed_flow: bool,
    pub quality_prioritization: VTFrameRateConversionConfigurationQualityPrioritization,
    pub revision: VTFrameRateConversionConfigurationRevision,
}

impl FrameRateConversionConfiguration {
    #[must_use]
    pub const fn new(frame_width: usize, frame_height: usize) -> Self {
        Self {
            frame_width,
            frame_height,
            use_precomputed_flow: false,
            quality_prioritization: VTFrameRateConversionConfigurationQualityPrioritization::Normal,
            revision: VTFrameRateConversionConfigurationRevision::Revision1,
        }
    }
}

/// Explicit configuration for `VTOpticalFlowConfiguration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpticalFlowConfiguration {
    pub frame_width: usize,
    pub frame_height: usize,
    pub quality_prioritization: VTOpticalFlowConfigurationQualityPrioritization,
    pub revision: VTOpticalFlowConfigurationRevision,
}

impl OpticalFlowConfiguration {
    #[must_use]
    pub const fn new(frame_width: usize, frame_height: usize) -> Self {
        Self {
            frame_width,
            frame_height,
            quality_prioritization: VTOpticalFlowConfigurationQualityPrioritization::Normal,
            revision: VTOpticalFlowConfigurationRevision::Revision1,
        }
    }
}

/// Super-resolution model availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum SuperResolutionModelStatus {
    DownloadRequired = 0,
    Downloading = 1,
    Ready = 2,
}

impl SuperResolutionModelStatus {
    fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(Self::DownloadRequired),
            1 => Some(Self::Downloading),
            2 => Some(Self::Ready),
            _ => None,
        }
    }
}

/// Query Apple's `VTFrameProcessor*Configuration.isSupported` for each wrapped effect.
#[must_use]
pub fn frame_processor_capabilities() -> FrameProcessorCapabilities {
    unsafe {
        FrameProcessorCapabilities {
            super_resolution: vt_super_resolution_is_supported(),
            motion_blur: vt_motion_blur_is_supported(),
            temporal_noise_filter: vt_temporal_noise_filter_is_supported(),
            frame_rate_conversion: vt_frame_rate_conversion_is_supported(),
            low_latency_super_resolution: vt_low_latency_super_resolution_is_supported(),
            low_latency_frame_interpolation: vt_low_latency_frame_interpolation_is_supported(),
            optical_flow: vt_optical_flow_is_supported(),
        }
    }
}

/// Integer scale factors accepted by `VTSuperResolutionScalerConfiguration`.
#[must_use]
pub fn super_resolution_supported_scale_factors() -> Vec<u32> {
    const MAX: usize = 16;
    let mut buf = vec![0u32; MAX];
    let n = unsafe { vt_super_resolution_supported_scale_factors(buf.as_mut_ptr(), MAX) };
    buf.truncate(n);
    buf
}

/// Floating-point scale factors accepted by `VTLowLatencySuperResolutionScalerConfiguration`
/// for a specific input size.
#[must_use]
pub fn low_latency_super_resolution_supported_scale_factors(
    frame_width: usize,
    frame_height: usize,
) -> Vec<f32> {
    const MAX: usize = 16;
    let mut buf = vec![0f32; MAX];
    let n = unsafe {
        vt_low_latency_super_resolution_supported_scale_factors(
            frame_width as isize,
            frame_height as isize,
            buf.as_mut_ptr(),
            MAX,
        )
    };
    buf.truncate(n);
    buf
}

/// Query the model-download state for a super-resolution configuration.
#[must_use]
pub fn super_resolution_model_status(
    frame_width: usize,
    frame_height: usize,
    scale_factor: usize,
    use_precomputed_flow: bool,
    input_is_image: bool,
) -> Option<SuperResolutionModelStatus> {
    super_resolution_model_status_for_configuration(default_super_resolution_configuration(
        frame_width,
        frame_height,
        scale_factor,
        use_precomputed_flow,
        input_is_image,
    ))
}

/// Query the model-download state for an explicit super-resolution configuration.
#[must_use]
pub fn super_resolution_model_status_for_configuration(
    configuration: SuperResolutionConfiguration,
) -> Option<SuperResolutionModelStatus> {
    SuperResolutionModelStatus::from_raw(unsafe {
        vt_super_resolution_model_status(
            configuration.frame_width as isize,
            configuration.frame_height as isize,
            configuration.scale_factor as isize,
            configuration.use_precomputed_flow,
            configuration.input_type as isize,
            configuration.quality_prioritization as isize,
            configuration.revision as isize,
        )
    })
}

/// Query the model-download percentage for a super-resolution configuration.
#[must_use]
pub fn super_resolution_model_percentage_available(
    frame_width: usize,
    frame_height: usize,
    scale_factor: usize,
    use_precomputed_flow: bool,
    input_is_image: bool,
) -> Option<f32> {
    super_resolution_model_percentage_available_for_configuration(
        default_super_resolution_configuration(
            frame_width,
            frame_height,
            scale_factor,
            use_precomputed_flow,
            input_is_image,
        ),
    )
}

/// Query the model-download percentage for an explicit super-resolution configuration.
#[must_use]
pub fn super_resolution_model_percentage_available_for_configuration(
    configuration: SuperResolutionConfiguration,
) -> Option<f32> {
    let value = unsafe {
        vt_super_resolution_model_percentage_available(
            configuration.frame_width as isize,
            configuration.frame_height as isize,
            configuration.scale_factor as isize,
            configuration.use_precomputed_flow,
            configuration.input_type as isize,
            configuration.quality_prioritization as isize,
            configuration.revision as isize,
        )
    };
    (value >= 0.0).then_some(value)
}

/// Trigger background model download for a super-resolution configuration and block
/// until the completion handler fires.
///
/// # Errors
///
/// Returns [`VTError::ApiFailed`] when `VideoToolbox` reports an error.
pub fn download_super_resolution_model(
    frame_width: usize,
    frame_height: usize,
    scale_factor: usize,
    use_precomputed_flow: bool,
    input_is_image: bool,
) -> Result<(), VTError> {
    download_super_resolution_model_for_configuration(default_super_resolution_configuration(
        frame_width,
        frame_height,
        scale_factor,
        use_precomputed_flow,
        input_is_image,
    ))
}

/// Trigger background model download for an explicit super-resolution configuration.
///
/// # Errors
///
/// Returns [`VTError::ApiFailed`] when `VideoToolbox` reports an error.
pub fn download_super_resolution_model_for_configuration(
    configuration: SuperResolutionConfiguration,
) -> Result<(), VTError> {
    let status = unsafe {
        vt_super_resolution_download_model(
            configuration.frame_width as isize,
            configuration.frame_height as isize,
            configuration.scale_factor as isize,
            configuration.use_precomputed_flow,
            configuration.input_type as isize,
            configuration.quality_prioritization as isize,
            configuration.revision as isize,
        )
    };
    api_result("downloadConfigurationModelWithCompletionHandler", status)
}

/// Retained `VTFrameProcessorFrame` wrapper.
pub struct FrameProcessorFrame {
    inner: *mut c_void,
    buffer: CVPixelBuffer,
    presentation_time_stamp: CMTime,
}

unsafe impl Send for FrameProcessorFrame {}
unsafe impl Sync for FrameProcessorFrame {}

impl Drop for FrameProcessorFrame {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { vt_frame_processor_frame_release(self.inner) };
            self.inner = core::ptr::null_mut();
        }
    }
}

impl FrameProcessorFrame {
    /// Wrap an IOSurface-backed `CVPixelBuffer` and presentation timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::InvalidArgument`] when the buffer is not backed by
    /// `IOSurface`, or [`VTError::ApiFailed`] if the Swift bridge rejects it.
    pub fn new(buffer: &CVPixelBuffer, presentation_time_stamp: CMTime) -> Result<Self, VTError> {
        if !buffer.is_backed_by_io_surface() {
            return Err(VTError::InvalidArgument(
                "VTFrameProcessorFrame requires an IOSurface-backed CVPixelBuffer".to_string(),
            ));
        }

        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_frame_processor_frame_create(
                buffer.as_ptr(),
                to_ffi_time(presentation_time_stamp),
                &mut out,
            )
        };
        if status != 0 || out.is_null() {
            return Err(VTError::ApiFailed {
                api: "VTFrameProcessorFrame.init(buffer:presentationTimeStamp:)",
                status,
            });
        }

        Ok(Self {
            inner: out,
            buffer: buffer.clone(),
            presentation_time_stamp,
        })
    }

    #[must_use]
    pub fn buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }

    #[must_use]
    pub const fn presentation_time_stamp(&self) -> CMTime {
        self.presentation_time_stamp
    }

    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.inner
    }
}

/// Retained `VTFrameProcessorOpticalFlow` wrapper.
pub struct FrameProcessorOpticalFlow {
    inner: *mut c_void,
    forward_flow: CVPixelBuffer,
    backward_flow: CVPixelBuffer,
}

unsafe impl Send for FrameProcessorOpticalFlow {}
unsafe impl Sync for FrameProcessorOpticalFlow {}

impl Drop for FrameProcessorOpticalFlow {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { vt_frame_processor_optical_flow_release(self.inner) };
            self.inner = core::ptr::null_mut();
        }
    }
}

impl FrameProcessorOpticalFlow {
    /// Wrap forward/backward IOSurface-backed flow buffers.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::InvalidArgument`] when either buffer is not backed by
    /// `IOSurface`, or [`VTError::ApiFailed`] if the Swift bridge rejects it.
    pub fn new(
        forward_flow: &CVPixelBuffer,
        backward_flow: &CVPixelBuffer,
    ) -> Result<Self, VTError> {
        if !forward_flow.is_backed_by_io_surface() || !backward_flow.is_backed_by_io_surface() {
            return Err(VTError::InvalidArgument(
                "VTFrameProcessorOpticalFlow requires IOSurface-backed CVPixelBuffers".to_string(),
            ));
        }

        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_frame_processor_optical_flow_create(
                forward_flow.as_ptr(),
                backward_flow.as_ptr(),
                &mut out,
            )
        };
        if status != 0 || out.is_null() {
            return Err(VTError::ApiFailed {
                api: "VTFrameProcessorOpticalFlow.init(forwardFlow:backwardFlow:)",
                status,
            });
        }

        Ok(Self {
            inner: out,
            forward_flow: forward_flow.clone(),
            backward_flow: backward_flow.clone(),
        })
    }

    #[must_use]
    pub fn forward_flow(&self) -> &CVPixelBuffer {
        &self.forward_flow
    }

    #[must_use]
    pub fn backward_flow(&self) -> &CVPixelBuffer {
        &self.backward_flow
    }

    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.inner
    }
}

/// Active `VTFrameProcessor` session for one specific pipeline.
pub struct FrameProcessor {
    inner: *mut c_void,
}

unsafe impl Send for FrameProcessor {}
unsafe impl Sync for FrameProcessor {}

impl Drop for FrameProcessor {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe {
                vt_frame_processor_end(self.inner);
                vt_frame_processor_release(self.inner);
            }
            self.inner = core::ptr::null_mut();
        }
    }
}

impl FrameProcessor {
    fn from_status(status: i32, ptr: *mut c_void) -> Result<Self, VTError> {
        if status != 0 || ptr.is_null() {
            return Err(VTError::SessionCreateFailed(status));
        }
        Ok(Self { inner: ptr })
    }

    /// Start a session configured for the super-resolution scaler.
    pub fn start_super_resolution(
        frame_width: usize,
        frame_height: usize,
        scale_factor: usize,
        use_precomputed_flow: bool,
        input_is_image: bool,
    ) -> Result<Self, VTError> {
        Self::start_super_resolution_with_configuration(default_super_resolution_configuration(
            frame_width,
            frame_height,
            scale_factor,
            use_precomputed_flow,
            input_is_image,
        ))
    }

    /// Start a session configured with the full `VTSuperResolutionScalerConfiguration` surface.
    pub fn start_super_resolution_with_configuration(
        configuration: SuperResolutionConfiguration,
    ) -> Result<Self, VTError> {
        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_super_resolution_start(
                configuration.frame_width as isize,
                configuration.frame_height as isize,
                configuration.scale_factor as isize,
                configuration.use_precomputed_flow,
                configuration.input_type as isize,
                configuration.quality_prioritization as isize,
                configuration.revision as isize,
                &mut out,
            )
        };
        Self::from_status(status, out)
    }

    /// Start a motion-blur session.
    pub fn start_motion_blur(
        frame_width: usize,
        frame_height: usize,
        use_precomputed_flow: bool,
    ) -> Result<Self, VTError> {
        let mut configuration = MotionBlurConfiguration::new(frame_width, frame_height);
        configuration.use_precomputed_flow = use_precomputed_flow;
        Self::start_motion_blur_with_configuration(configuration)
    }

    /// Start a motion-blur session with the full `VTMotionBlurConfiguration` surface.
    pub fn start_motion_blur_with_configuration(
        configuration: MotionBlurConfiguration,
    ) -> Result<Self, VTError> {
        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_motion_blur_start(
                configuration.frame_width as isize,
                configuration.frame_height as isize,
                configuration.use_precomputed_flow,
                configuration.quality_prioritization as isize,
                configuration.revision as isize,
                &mut out,
            )
        };
        Self::from_status(status, out)
    }

    /// Start a temporal-noise-filter session.
    pub fn start_temporal_noise_filter(
        frame_width: usize,
        frame_height: usize,
        source_pixel_format: u32,
    ) -> Result<Self, VTError> {
        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_temporal_noise_filter_start(
                frame_width as isize,
                frame_height as isize,
                source_pixel_format,
                &mut out,
            )
        };
        Self::from_status(status, out)
    }

    /// Start a frame-rate-conversion session.
    pub fn start_frame_rate_conversion(
        frame_width: usize,
        frame_height: usize,
        use_precomputed_flow: bool,
    ) -> Result<Self, VTError> {
        let mut configuration = FrameRateConversionConfiguration::new(frame_width, frame_height);
        configuration.use_precomputed_flow = use_precomputed_flow;
        Self::start_frame_rate_conversion_with_configuration(configuration)
    }

    /// Start a frame-rate-conversion session with the full `VTFrameRateConversionConfiguration` surface.
    pub fn start_frame_rate_conversion_with_configuration(
        configuration: FrameRateConversionConfiguration,
    ) -> Result<Self, VTError> {
        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_frame_rate_conversion_start(
                configuration.frame_width as isize,
                configuration.frame_height as isize,
                configuration.use_precomputed_flow,
                configuration.quality_prioritization as isize,
                configuration.revision as isize,
                &mut out,
            )
        };
        Self::from_status(status, out)
    }

    /// Start a low-latency super-resolution session.
    pub fn start_low_latency_super_resolution(
        frame_width: usize,
        frame_height: usize,
        scale_factor: f32,
    ) -> Result<Self, VTError> {
        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_low_latency_super_resolution_start(
                frame_width as isize,
                frame_height as isize,
                scale_factor,
                &mut out,
            )
        };
        Self::from_status(status, out)
    }

    /// Start a low-latency frame-interpolation session.
    pub fn start_low_latency_frame_interpolation(
        frame_width: usize,
        frame_height: usize,
        number_of_interpolated_frames: usize,
    ) -> Result<Self, VTError> {
        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_low_latency_frame_interpolation_start(
                frame_width as isize,
                frame_height as isize,
                number_of_interpolated_frames as isize,
                &mut out,
            )
        };
        Self::from_status(status, out)
    }

    /// Start an optical-flow session.
    pub fn start_optical_flow(frame_width: usize, frame_height: usize) -> Result<Self, VTError> {
        Self::start_optical_flow_with_configuration(OpticalFlowConfiguration::new(
            frame_width,
            frame_height,
        ))
    }

    /// Start an optical-flow session with the full `VTOpticalFlowConfiguration` surface.
    pub fn start_optical_flow_with_configuration(
        configuration: OpticalFlowConfiguration,
    ) -> Result<Self, VTError> {
        let mut out = core::ptr::null_mut();
        let status = unsafe {
            vt_optical_flow_start(
                configuration.frame_width as isize,
                configuration.frame_height as isize,
                configuration.quality_prioritization as isize,
                configuration.revision as isize,
                &mut out,
            )
        };
        Self::from_status(status, out)
    }

    /// Explicitly end the session before drop.
    pub fn end_session(&self) {
        unsafe { vt_frame_processor_end(self.inner) };
    }

    /// Process one super-resolution submission.
    pub fn process_super_resolution(
        &self,
        source_frame: &FrameProcessorFrame,
        previous_frame: Option<&FrameProcessorFrame>,
        previous_output_frame: Option<&FrameProcessorFrame>,
        optical_flow: Option<&FrameProcessorOpticalFlow>,
        submission_mode: FrameProcessorSubmissionMode,
        destination_frame: &FrameProcessorFrame,
    ) -> Result<(), VTError> {
        api_result("VTSuperResolutionScalerParameters", unsafe {
            vt_frame_processor_process_super_resolution(
                self.inner,
                source_frame.as_ptr(),
                frame_ptr(previous_frame),
                frame_ptr(previous_output_frame),
                flow_ptr(optical_flow),
                submission_mode as i32,
                destination_frame.as_ptr(),
            )
        })
    }

    /// Queue one super-resolution submission into an existing Metal command buffer.
    pub fn process_super_resolution_with_command_buffer(
        &self,
        command_buffer: &CommandBuffer,
        source_frame: &FrameProcessorFrame,
        previous_frame: Option<&FrameProcessorFrame>,
        previous_output_frame: Option<&FrameProcessorFrame>,
        optical_flow: Option<&FrameProcessorOpticalFlow>,
        submission_mode: FrameProcessorSubmissionMode,
        destination_frame: &FrameProcessorFrame,
    ) -> Result<(), VTError> {
        api_result("process(with:parameters:) super-resolution", unsafe {
            vt_frame_processor_process_super_resolution_with_command_buffer(
                self.inner,
                command_buffer.as_ptr(),
                source_frame.as_ptr(),
                frame_ptr(previous_frame),
                frame_ptr(previous_output_frame),
                flow_ptr(optical_flow),
                submission_mode as i32,
                destination_frame.as_ptr(),
            )
        })
    }

    /// Process one motion-blur submission.
    #[allow(clippy::too_many_arguments)]
    pub fn process_motion_blur(
        &self,
        source_frame: &FrameProcessorFrame,
        next_frame: Option<&FrameProcessorFrame>,
        previous_frame: Option<&FrameProcessorFrame>,
        next_optical_flow: Option<&FrameProcessorOpticalFlow>,
        previous_optical_flow: Option<&FrameProcessorOpticalFlow>,
        motion_blur_strength: usize,
        submission_mode: FrameProcessorSubmissionMode,
        destination_frame: &FrameProcessorFrame,
    ) -> Result<(), VTError> {
        api_result("VTMotionBlurParameters", unsafe {
            vt_frame_processor_process_motion_blur(
                self.inner,
                source_frame.as_ptr(),
                frame_ptr(next_frame),
                frame_ptr(previous_frame),
                flow_ptr(next_optical_flow),
                flow_ptr(previous_optical_flow),
                motion_blur_strength as isize,
                submission_mode as i32,
                destination_frame.as_ptr(),
            )
        })
    }

    /// Queue one motion-blur submission into an existing Metal command buffer.
    #[allow(clippy::too_many_arguments)]
    pub fn process_motion_blur_with_command_buffer(
        &self,
        command_buffer: &CommandBuffer,
        source_frame: &FrameProcessorFrame,
        next_frame: Option<&FrameProcessorFrame>,
        previous_frame: Option<&FrameProcessorFrame>,
        next_optical_flow: Option<&FrameProcessorOpticalFlow>,
        previous_optical_flow: Option<&FrameProcessorOpticalFlow>,
        motion_blur_strength: usize,
        submission_mode: FrameProcessorSubmissionMode,
        destination_frame: &FrameProcessorFrame,
    ) -> Result<(), VTError> {
        api_result("process(with:parameters:) motion-blur", unsafe {
            vt_frame_processor_process_motion_blur_with_command_buffer(
                self.inner,
                command_buffer.as_ptr(),
                source_frame.as_ptr(),
                frame_ptr(next_frame),
                frame_ptr(previous_frame),
                flow_ptr(next_optical_flow),
                flow_ptr(previous_optical_flow),
                motion_blur_strength as isize,
                submission_mode as i32,
                destination_frame.as_ptr(),
            )
        })
    }

    /// Process one temporal-noise-filter submission.
    pub fn process_temporal_noise_filter(
        &self,
        source_frame: &FrameProcessorFrame,
        next_frames: &[FrameProcessorFrame],
        previous_frames: &[FrameProcessorFrame],
        destination_frame: &FrameProcessorFrame,
        filter_strength: f32,
        has_discontinuity: bool,
    ) -> Result<(), VTError> {
        let next_ptrs = frame_slice_ptrs(next_frames);
        let previous_ptrs = frame_slice_ptrs(previous_frames);
        api_result("VTTemporalNoiseFilterParameters", unsafe {
            vt_frame_processor_process_temporal_noise_filter(
                self.inner,
                source_frame.as_ptr(),
                next_ptrs.as_ptr(),
                next_ptrs.len(),
                previous_ptrs.as_ptr(),
                previous_ptrs.len(),
                destination_frame.as_ptr(),
                filter_strength,
                has_discontinuity,
            )
        })
    }

    /// Queue one temporal-noise-filter submission into an existing Metal command buffer.
    pub fn process_temporal_noise_filter_with_command_buffer(
        &self,
        command_buffer: &CommandBuffer,
        source_frame: &FrameProcessorFrame,
        next_frames: &[FrameProcessorFrame],
        previous_frames: &[FrameProcessorFrame],
        destination_frame: &FrameProcessorFrame,
        filter_strength: f32,
        has_discontinuity: bool,
    ) -> Result<(), VTError> {
        let next_ptrs = frame_slice_ptrs(next_frames);
        let previous_ptrs = frame_slice_ptrs(previous_frames);
        api_result("process(with:parameters:) temporal-noise-filter", unsafe {
            vt_frame_processor_process_temporal_noise_filter_with_command_buffer(
                self.inner,
                command_buffer.as_ptr(),
                source_frame.as_ptr(),
                next_ptrs.as_ptr(),
                next_ptrs.len(),
                previous_ptrs.as_ptr(),
                previous_ptrs.len(),
                destination_frame.as_ptr(),
                filter_strength,
                has_discontinuity,
            )
        })
    }

    /// Process one frame-rate-conversion submission.
    pub fn process_frame_rate_conversion(
        &self,
        source_frame: &FrameProcessorFrame,
        next_frame: Option<&FrameProcessorFrame>,
        optical_flow: Option<&FrameProcessorOpticalFlow>,
        interpolation_phase: &[f32],
        submission_mode: FrameRateConversionSubmissionMode,
        destination_frames: &[FrameProcessorFrame],
    ) -> Result<(), VTError> {
        validate_parallel_lengths(
            "interpolation_phase",
            interpolation_phase,
            destination_frames,
        )?;
        let destination_ptrs = frame_slice_ptrs(destination_frames);
        api_result("VTFrameRateConversionParameters", unsafe {
            vt_frame_processor_process_frame_rate_conversion(
                self.inner,
                source_frame.as_ptr(),
                frame_ptr(next_frame),
                flow_ptr(optical_flow),
                interpolation_phase.as_ptr(),
                interpolation_phase.len(),
                submission_mode as i32,
                destination_ptrs.as_ptr(),
                destination_ptrs.len(),
            )
        })
    }

    /// Queue one frame-rate-conversion submission into an existing Metal command buffer.
    pub fn process_frame_rate_conversion_with_command_buffer(
        &self,
        command_buffer: &CommandBuffer,
        source_frame: &FrameProcessorFrame,
        next_frame: Option<&FrameProcessorFrame>,
        optical_flow: Option<&FrameProcessorOpticalFlow>,
        interpolation_phase: &[f32],
        submission_mode: FrameRateConversionSubmissionMode,
        destination_frames: &[FrameProcessorFrame],
    ) -> Result<(), VTError> {
        validate_parallel_lengths(
            "interpolation_phase",
            interpolation_phase,
            destination_frames,
        )?;
        let destination_ptrs = frame_slice_ptrs(destination_frames);
        api_result("process(with:parameters:) frame-rate-conversion", unsafe {
            vt_frame_processor_process_frame_rate_conversion_with_command_buffer(
                self.inner,
                command_buffer.as_ptr(),
                source_frame.as_ptr(),
                frame_ptr(next_frame),
                flow_ptr(optical_flow),
                interpolation_phase.as_ptr(),
                interpolation_phase.len(),
                submission_mode as i32,
                destination_ptrs.as_ptr(),
                destination_ptrs.len(),
            )
        })
    }

    /// Process one low-latency super-resolution submission.
    pub fn process_low_latency_super_resolution(
        &self,
        source_frame: &FrameProcessorFrame,
        destination_frame: &FrameProcessorFrame,
    ) -> Result<(), VTError> {
        api_result("VTLowLatencySuperResolutionScalerParameters", unsafe {
            vt_frame_processor_process_low_latency_super_resolution(
                self.inner,
                source_frame.as_ptr(),
                destination_frame.as_ptr(),
            )
        })
    }

    /// Queue one low-latency super-resolution submission into an existing Metal command buffer.
    pub fn process_low_latency_super_resolution_with_command_buffer(
        &self,
        command_buffer: &CommandBuffer,
        source_frame: &FrameProcessorFrame,
        destination_frame: &FrameProcessorFrame,
    ) -> Result<(), VTError> {
        api_result(
            "process(with:parameters:) low-latency-super-resolution",
            unsafe {
                vt_frame_processor_process_low_latency_super_resolution_with_command_buffer(
                    self.inner,
                    command_buffer.as_ptr(),
                    source_frame.as_ptr(),
                    destination_frame.as_ptr(),
                )
            },
        )
    }

    /// Process one low-latency frame-interpolation submission.
    pub fn process_low_latency_frame_interpolation(
        &self,
        source_frame: &FrameProcessorFrame,
        previous_frame: &FrameProcessorFrame,
        interpolation_phase: &[f32],
        destination_frames: &[FrameProcessorFrame],
    ) -> Result<(), VTError> {
        validate_parallel_lengths(
            "interpolation_phase",
            interpolation_phase,
            destination_frames,
        )?;
        let destination_ptrs = frame_slice_ptrs(destination_frames);
        api_result("VTLowLatencyFrameInterpolationParameters", unsafe {
            vt_frame_processor_process_low_latency_frame_interpolation(
                self.inner,
                source_frame.as_ptr(),
                previous_frame.as_ptr(),
                interpolation_phase.as_ptr(),
                interpolation_phase.len(),
                destination_ptrs.as_ptr(),
                destination_ptrs.len(),
            )
        })
    }

    /// Queue one low-latency frame-interpolation submission into an existing Metal command buffer.
    pub fn process_low_latency_frame_interpolation_with_command_buffer(
        &self,
        command_buffer: &CommandBuffer,
        source_frame: &FrameProcessorFrame,
        previous_frame: &FrameProcessorFrame,
        interpolation_phase: &[f32],
        destination_frames: &[FrameProcessorFrame],
    ) -> Result<(), VTError> {
        validate_parallel_lengths(
            "interpolation_phase",
            interpolation_phase,
            destination_frames,
        )?;
        let destination_ptrs = frame_slice_ptrs(destination_frames);
        api_result(
            "process(with:parameters:) low-latency-frame-interpolation",
            unsafe {
                vt_frame_processor_process_low_latency_frame_interpolation_with_command_buffer(
                    self.inner,
                    command_buffer.as_ptr(),
                    source_frame.as_ptr(),
                    previous_frame.as_ptr(),
                    interpolation_phase.as_ptr(),
                    interpolation_phase.len(),
                    destination_ptrs.as_ptr(),
                    destination_ptrs.len(),
                )
            },
        )
    }

    /// Process one optical-flow submission.
    pub fn process_optical_flow(
        &self,
        source_frame: &FrameProcessorFrame,
        next_frame: &FrameProcessorFrame,
        submission_mode: FrameProcessorSubmissionMode,
        destination_optical_flow: &FrameProcessorOpticalFlow,
    ) -> Result<(), VTError> {
        api_result("VTOpticalFlowParameters", unsafe {
            vt_frame_processor_process_optical_flow(
                self.inner,
                source_frame.as_ptr(),
                next_frame.as_ptr(),
                submission_mode as i32,
                destination_optical_flow.as_ptr(),
            )
        })
    }

    /// Queue one optical-flow submission into an existing Metal command buffer.
    pub fn process_optical_flow_with_command_buffer(
        &self,
        command_buffer: &CommandBuffer,
        source_frame: &FrameProcessorFrame,
        next_frame: &FrameProcessorFrame,
        submission_mode: FrameProcessorSubmissionMode,
        destination_optical_flow: &FrameProcessorOpticalFlow,
    ) -> Result<(), VTError> {
        api_result("process(with:parameters:) optical-flow", unsafe {
            vt_frame_processor_process_optical_flow_with_command_buffer(
                self.inner,
                command_buffer.as_ptr(),
                source_frame.as_ptr(),
                next_frame.as_ptr(),
                submission_mode as i32,
                destination_optical_flow.as_ptr(),
            )
        })
    }

    /// Raw `VTFrameProcessor` pointer.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut c_void {
        self.inner
    }
}

fn default_super_resolution_configuration(
    frame_width: usize,
    frame_height: usize,
    scale_factor: usize,
    use_precomputed_flow: bool,
    input_is_image: bool,
) -> SuperResolutionConfiguration {
    let mut configuration =
        SuperResolutionConfiguration::new(frame_width, frame_height, scale_factor);
    configuration.use_precomputed_flow = use_precomputed_flow;
    configuration.input_type = if input_is_image {
        VTSuperResolutionScalerConfigurationInputType::Image
    } else {
        VTSuperResolutionScalerConfigurationInputType::Video
    };
    configuration
}

fn to_ffi_time(time: CMTime) -> ffi::CMTime {
    time
}

fn frame_ptr(frame: Option<&FrameProcessorFrame>) -> *mut c_void {
    frame.map_or(core::ptr::null_mut(), FrameProcessorFrame::as_ptr)
}

fn flow_ptr(flow: Option<&FrameProcessorOpticalFlow>) -> *mut c_void {
    flow.map_or(core::ptr::null_mut(), FrameProcessorOpticalFlow::as_ptr)
}

fn frame_slice_ptrs(frames: &[FrameProcessorFrame]) -> Vec<*mut c_void> {
    frames.iter().map(FrameProcessorFrame::as_ptr).collect()
}

fn validate_parallel_lengths<T>(
    label: &'static str,
    left: &[f32],
    right: &[T],
) -> Result<(), VTError> {
    if left.len() == right.len() {
        Ok(())
    } else {
        Err(VTError::InvalidArgument(format!(
            "{label} length ({}) must match destination frame count ({})",
            left.len(),
            right.len()
        )))
    }
}

fn api_result(api: &'static str, status: i32) -> Result<(), VTError> {
    if status == 0 {
        Ok(())
    } else {
        Err(VTError::ApiFailed { api, status })
    }
}
