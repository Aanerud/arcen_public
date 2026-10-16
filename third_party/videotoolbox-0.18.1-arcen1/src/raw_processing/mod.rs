#![allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]

//! `VTRAWProcessingSession` — `ProRes` RAW / `CinemaDNG` decoder with
//! per-frame parameter controls (macOS 15+).
//!
//! Session creation / parameter readback / parameter writeback /
//! complete-frames all go through the pure-C FFI. The Swift bridge wraps
//! the async `process(frame:)` API for both blocking and future-based Rust
//! entry points.

use core::ffi::c_void;
use core::ptr;
use std::sync::Mutex;

use apple_cf::{cf::CFType, cm::CMFormatDescription, cv::CVPixelBuffer};
#[cfg(feature = "async")]
use doom_fish_utils::completion::{AsyncCompletion, SyncCompletionPtr};
#[cfg(feature = "async")]
use doom_fish_utils::panic_safe::catch_user_panic;

use crate::error::VTError;
use crate::ffi;
use crate::session;

#[cfg(feature = "async")]
type RawProcessFrameAsyncCallback = unsafe extern "C" fn(*mut c_void, i32, *mut c_void);

extern "C" {
    fn vtb_raw_session_process_frame(
        session: *mut c_void,
        input_pixel_buffer: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    #[cfg(feature = "async")]
    fn vtb_raw_session_process_frame_async(
        session: *mut c_void,
        input_pixel_buffer: *mut c_void,
        refcon: *mut c_void,
        callback: Option<RawProcessFrameAsyncCallback>,
    ) -> i32;
    fn vtb_raw_session_set_parameter_changed_handler(
        session: *mut c_void,
        refcon: *mut c_void,
        callback: Option<unsafe extern "C" fn(*mut c_void, ffi::CFArrayRef)>,
    ) -> i32;
}

type ParameterChangedCallback = Box<dyn FnMut(Vec<RawProcessingParameter>) + Send + 'static>;

#[cfg(feature = "async")]
struct AsyncRawProcessingContext(SyncCompletionPtr);

/// `VTRAWProcessingSessionRef`.
pub struct RawProcessingSession {
    inner: ffi::VTRAWProcessingSessionRef,
    parameter_changed_handler: Mutex<Option<*mut ParameterChangedCallback>>,
}

unsafe impl Send for RawProcessingSession {}
unsafe impl Sync for RawProcessingSession {}

impl Drop for RawProcessingSession {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            let raw = match self.parameter_changed_handler.lock() {
                Ok(mut handler) => handler.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            if let Some(raw) = raw {
                unsafe {
                    let _ = vtb_raw_session_set_parameter_changed_handler(
                        self.inner.cast(),
                        ptr::null_mut(),
                        None,
                    );
                    drop(Box::from_raw(raw));
                }
            }
            unsafe {
                ffi::VTRAWProcessingSessionInvalidate(self.inner);
                ffi::CFRelease(self.inner.cast());
            }
            self.inner = ptr::null_mut();
        }
    }
}

impl RawProcessingSession {
    /// CoreFoundation type identifier for `VTRAWProcessingSession`.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { ffi::VTRAWProcessingSessionGetTypeID() }
    }

    /// Create a RAW processing session for the given video format.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SessionCreateFailed`] on failure.
    pub fn new(format: &CMFormatDescription) -> Result<Self, VTError> {
        let mut p: ffi::VTRAWProcessingSessionRef = ptr::null_mut();
        let s = unsafe {
            ffi::VTRAWProcessingSessionCreate(
                ffi::kCFAllocatorDefault,
                format.as_ptr().cast::<c_void>(),
                ptr::null(),
                ptr::null(),
                &mut p,
            )
        };
        if s != 0 || p.is_null() {
            return Err(VTError::SessionCreateFailed(s));
        }
        Ok(Self {
            inner: p,
            parameter_changed_handler: Mutex::new(None),
        })
    }

    /// Force-complete any outstanding frames.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn complete_frames(&self) -> Result<(), VTError> {
        let s = unsafe { ffi::VTRAWProcessingSessionCompleteFrames(self.inner) };
        if s == 0 {
            Ok(())
        } else {
            Err(VTError::EncodeFailed(s))
        }
    }

    /// Copy a raw `VTSession` property from the underlying RAW processing session.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if `VTSessionCopyProperty` fails.
    ///
    /// # Safety
    ///
    /// `key` must be a valid `CFStringRef` exported by `VideoToolbox`.
    pub unsafe fn copy_property(&self, key: ffi::CFStringRef) -> Result<Option<CFType>, VTError> {
        unsafe { session::copy_property(self.inner.cast(), key) }
    }

    /// Copy the sidecar-file metadata blob, when the active RAW processor exposes it.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if `VTSessionCopyProperty` fails.
    pub fn metadata_for_sidecar_file(&self) -> Result<Option<CFType>, VTError> {
        unsafe { self.copy_property(ffi::kVTRAWProcessingPropertyKey_MetadataForSidecarFile) }
    }

    /// Copy the requested Metal-device registry ID, when the active RAW processor exposes it.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if `VTSessionCopyProperty` fails.
    pub fn metal_device_registry_id(&self) -> Result<Option<CFType>, VTError> {
        unsafe { self.copy_property(ffi::kVTRAWProcessingPropertyKey_MetalDeviceRegistryID) }
    }

    /// Copy the output color-attachment dictionary, when the active RAW processor exposes it.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if `VTSessionCopyProperty` fails.
    pub fn output_color_attachments(&self) -> Result<Option<CFType>, VTError> {
        unsafe { self.copy_property(ffi::kVTRAWProcessingPropertyKey_OutputColorAttachments) }
    }

    /// Copy the array of processing parameters this RAW codec
    /// exposes.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn parameters(&self) -> Result<Vec<RawProcessingParameter>, VTError> {
        let mut arr: ffi::CFArrayRef = ptr::null();
        let s =
            unsafe { ffi::VTRAWProcessingSessionCopyProcessingParameters(self.inner, &mut arr) };
        if s != 0 || arr.is_null() {
            return Err(VTError::EncodeFailed(s));
        }
        let out = parameters_from_array(arr);
        unsafe { ffi::CFRelease(arr.cast()) };
        Ok(out)
    }

    /// Set processing parameters. The dictionary keys must match the
    /// `kVTRAWProcessingParameter_Key` strings returned by
    /// [`parameters`](Self::parameters); values must conform to the
    /// declared value type.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    ///
    /// # Safety
    ///
    /// `params` must be a valid `CFDictionaryRef`.
    pub unsafe fn set_parameters_raw(&self, params: ffi::CFDictionaryRef) -> Result<(), VTError> {
        let s = unsafe { ffi::VTRAWProcessingSessionSetProcessingParameters(self.inner, params) };
        if s == 0 {
            Ok(())
        } else {
            Err(VTError::EncodeFailed(s))
        }
    }

    /// Install a callback that fires when the RAW processor changes its
    /// available parameters or their current values.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] when the framework rejects the handler.
    pub fn set_parameter_changed_handler<F>(&self, callback: F) -> Result<(), VTError>
    where
        F: FnMut(Vec<RawProcessingParameter>) + Send + 'static,
    {
        let raw = Box::into_raw(Box::new(Box::new(callback) as ParameterChangedCallback));
        let status = unsafe {
            vtb_raw_session_set_parameter_changed_handler(
                self.inner.cast(),
                raw.cast(),
                Some(raw_parameter_changed_trampoline),
            )
        };
        if status != 0 {
            unsafe { drop(Box::from_raw(raw)) };
            return Err(VTError::ApiFailed {
                api: "VTRAWProcessingSessionSetParameterChangedHandler",
                status,
            });
        }

        let old = match self.parameter_changed_handler.lock() {
            Ok(mut slot) => slot.replace(raw),
            Err(poisoned) => poisoned.into_inner().replace(raw),
        };
        if let Some(old) = old {
            unsafe { drop(Box::from_raw(old)) };
        }
        Ok(())
    }

    /// Remove any previously-installed parameter-change handler.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] when the framework rejects the update.
    pub fn clear_parameter_changed_handler(&self) -> Result<(), VTError> {
        let status = unsafe {
            vtb_raw_session_set_parameter_changed_handler(self.inner.cast(), ptr::null_mut(), None)
        };
        if status != 0 {
            return Err(VTError::ApiFailed {
                api: "VTRAWProcessingSessionSetParameterChangedHandler",
                status,
            });
        }

        let old = match self.parameter_changed_handler.lock() {
            Ok(mut handler) => handler.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(old) = old {
            unsafe { drop(Box::from_raw(old)) };
        }
        Ok(())
    }

    /// Process a single RAW input frame, returning the processed
    /// `CVPixelBuffer`. Blocks on Swift's `process(frame:)` async
    /// call internally.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn process(&self, input: &CVPixelBuffer) -> Result<CVPixelBuffer, VTError> {
        let mut out: *mut c_void = ptr::null_mut();
        let s = unsafe {
            vtb_raw_session_process_frame(self.inner, input.as_ptr().cast::<c_void>(), &mut out)
        };
        if s != 0 || out.is_null() {
            return Err(VTError::EncodeFailed(s));
        }
        CVPixelBuffer::from_raw(out.cast()).ok_or(VTError::EncodeFailed(0))
    }

    /// Submit `input_pixel_buffer` for processing and await the processed output.
    ///
    /// This method requires the crate's `async` feature.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] if the RAW processor rejects the frame
    /// or completes without a processed `CVPixelBuffer`.
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    pub async fn process_frame_async(
        &self,
        input_pixel_buffer: CVPixelBuffer,
    ) -> Result<CVPixelBuffer, VTError> {
        let (future, completion) = AsyncCompletion::<CVPixelBuffer>::create();
        let context = Box::into_raw(Box::new(AsyncRawProcessingContext(completion)));
        let status = unsafe {
            vtb_raw_session_process_frame_async(
                self.inner.cast(),
                input_pixel_buffer.as_ptr().cast::<c_void>(),
                context.cast::<c_void>(),
                Some(raw_process_async_trampoline),
            )
        };
        if status != 0 {
            let context = unsafe { Box::from_raw(context) };
            unsafe {
                AsyncCompletion::<CVPixelBuffer>::complete_err(context.0, status.to_string());
            };
            return Err(VTError::EncodeFailed(status));
        }

        future
            .await
            .map_err(|error| VTError::EncodeFailed(parse_async_status(&error)))
    }

    /// Raw `VTRAWProcessingSessionRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> ffi::VTRAWProcessingSessionRef {
        self.inner
    }
}

fn parameters_from_array(arr: ffi::CFArrayRef) -> Vec<RawProcessingParameter> {
    if arr.is_null() {
        return Vec::new();
    }

    let count = unsafe { ffi::CFArrayGetCount(arr) };
    let mut out = Vec::with_capacity(count.max(0) as usize);
    for i in 0..count {
        let dict = unsafe { ffi::CFArrayGetValueAtIndex(arr, i) };
        if !dict.is_null() {
            unsafe { ffi::CFRetain(dict.cast()) };
            out.push(RawProcessingParameter { dict: dict.cast() });
        }
    }
    out
}

unsafe extern "C" fn raw_parameter_changed_trampoline(
    refcon: *mut c_void,
    parameter_array: ffi::CFArrayRef,
) {
    // The user-supplied closure may panic. Unwinding across this `extern "C"`
    // boundary back into VideoToolbox is undefined behaviour, so the
    // invocation is wrapped in `catch_unwind`. `AssertUnwindSafe` is required
    // because the captured `&mut` callback is not `UnwindSafe`; the user owns
    // their own state consistency on panic.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(callback) = (unsafe { refcon.cast::<ParameterChangedCallback>().as_mut() }) else {
            return;
        };
        callback(parameters_from_array(parameter_array));
    }));
}

#[cfg(feature = "async")]
fn parse_async_status(error: &str) -> ffi::OSStatus {
    error.parse().unwrap_or(-1)
}

#[cfg(feature = "async")]
unsafe extern "C" fn raw_process_async_trampoline(
    refcon: *mut c_void,
    status: i32,
    output_pixel_buffer: *mut c_void,
) {
    catch_user_panic("videotoolbox::raw_processing::process_frame_async", || {
        let context = unsafe { Box::from_raw(refcon.cast::<AsyncRawProcessingContext>()) };
        if status != 0 {
            unsafe {
                AsyncCompletion::<CVPixelBuffer>::complete_err(context.0, status.to_string());
            };
            return;
        }

        let Some(pixel_buffer) = CVPixelBuffer::from_raw(output_pixel_buffer.cast()) else {
            unsafe {
                AsyncCompletion::<CVPixelBuffer>::complete_err(context.0, "-1".into());
            };
            return;
        };

        unsafe {
            AsyncCompletion::<CVPixelBuffer>::complete_ok(context.0, pixel_buffer);
        };
    });
}

/// A single RAW-processing parameter descriptor.
///
/// Wraps a `CFDictionary` returned by `VTRAWProcessingSession::parameters`.
pub struct RawProcessingParameter {
    dict: ffi::CFDictionaryRef,
}

unsafe impl Send for RawProcessingParameter {}
unsafe impl Sync for RawProcessingParameter {}

crate::utils::retained::vt_retained!(
    RawProcessingParameter,
    field = dict,
    release = ffi::CFRelease
);

impl RawProcessingParameter {
    /// Raw `CFDictionaryRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> ffi::CFDictionaryRef {
        self.dict
    }

    /// Stable identifier used as the key when writing back via
    /// [`RawProcessingSession::set_parameters_raw`].
    #[must_use]
    pub fn key(&self) -> Option<String> {
        unsafe { self.cf_string(ffi::kVTRAWProcessingParameter_Key) }
    }

    /// Localised, human-readable name.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        unsafe { self.cf_string(ffi::kVTRAWProcessingParameter_Name) }
    }

    /// Long-form description.
    #[must_use]
    pub fn description(&self) -> Option<String> {
        unsafe { self.cf_string(ffi::kVTRAWProcessingParameter_Description) }
    }

    /// `kVTRAWProcessingParameterValueType_*` discriminator.
    #[must_use]
    pub fn value_type(&self) -> Option<String> {
        unsafe { self.cf_string(ffi::kVTRAWProcessingParameter_ValueType) }
    }

    /// Current value (numeric).
    #[must_use]
    pub fn current_value(&self) -> Option<f64> {
        unsafe { self.cf_f64(ffi::kVTRAWProcessingParameter_CurrentValue) }
    }

    /// Minimum value (numeric).
    #[must_use]
    pub fn minimum_value(&self) -> Option<f64> {
        unsafe { self.cf_f64(ffi::kVTRAWProcessingParameter_MinimumValue) }
    }

    /// Maximum value (numeric).
    #[must_use]
    pub fn maximum_value(&self) -> Option<f64> {
        unsafe { self.cf_f64(ffi::kVTRAWProcessingParameter_MaximumValue) }
    }

    /// Initial/default value.
    #[must_use]
    pub fn initial_value(&self) -> Option<f64> {
        unsafe { self.cf_f64(ffi::kVTRAWProcessingParameter_InitialValue) }
    }

    /// Camera-captured value.
    #[must_use]
    pub fn camera_value(&self) -> Option<f64> {
        unsafe { self.cf_f64(ffi::kVTRAWProcessingParameter_CameraValue) }
    }

    /// Neutral (no-op) value.
    #[must_use]
    pub fn neutral_value(&self) -> Option<f64> {
        unsafe { self.cf_f64(ffi::kVTRAWProcessingParameter_NeutralValue) }
    }

    unsafe fn cf_string(&self, key: ffi::CFStringRef) -> Option<String> {
        let v = unsafe { ffi::CFDictionaryGetValue(self.dict, key.cast()) };
        if v.is_null() {
            return None;
        }
        let len = unsafe { ffi::CFStringGetLength(v.cast()) };
        if len < 0 {
            return None;
        }
        let cap = (len as usize).saturating_mul(4) + 1;
        let mut buf = vec![0u8; cap];
        let ok = unsafe {
            ffi::CFStringGetCString(
                v.cast(),
                buf.as_mut_ptr().cast(),
                cap as isize,
                ffi::kCFStringEncodingUTF8,
            )
        };
        if !ok {
            return None;
        }
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(cap);
        Some(String::from_utf8_lossy(&buf[..nul]).into_owned())
    }

    unsafe fn cf_f64(&self, key: ffi::CFStringRef) -> Option<f64> {
        let v = unsafe { ffi::CFDictionaryGetValue(self.dict, key.cast()) };
        if v.is_null() {
            return None;
        }
        let mut out: f64 = 0.0;
        let ok = unsafe {
            ffi::CFNumberGetValue(v.cast(), ffi::kCFNumberFloat64Type, (&raw mut out).cast())
        };
        if ok {
            Some(out)
        } else {
            None
        }
    }
}
