// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Microphone capture behind `DeckServices::start_microphone`.
//!
//! The macOS Deck's `MicrophoneCapture` on WASAPI: a capture thread owns the
//! device stream, the shared normaliser (`arcen_media::audio::
//! capture_normalizer`) turns the device's own mix format into
//! microphone-v1 frames, and the start-up waits for the stream (bounded at
//! 120 s, returning promptly once cancelled). The consent model is the
//! engine's and is unchanged: forwarding is opt-in per launch and per Pier
//! request; this module only captures when asked.
//!
//! **Permission.** A desktop app on Windows gets no per-app prompt on most
//! builds; access follows the Privacy & security › Microphone switches.
//! Capture checks them first ([`microphone_privacy`]) and also reads an
//! access-denied failure from the audio client as a refusal, so the Pier
//! hears `permission_denied` (the shared reason) and the Deck can show
//! [`PRIVACY_GUIDANCE`] with a link to the settings page, instead of
//! sending silence. A switch turned off during capture ends it as
//! `PermissionLost`, as revoking access on macOS does.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arcen_deck_core::microphone::{
    CapturedMicrophoneFrame, MicrophoneCancel, MicrophoneCaptureError, MicrophoneFrameSink,
    MicrophoneStream,
};
use arcen_media::audio::capture_normalizer::{
    MicrophoneCaptureNormalizer, NormalizedMicrophoneFrame,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::platform::registry::{Hive, read_string};

/// What the Deck shows when Windows refuses the microphone.
pub const PRIVACY_GUIDANCE: &str = "Windows is blocking microphone access for Arcen Deck. \
     Turn on Microphone access and Let desktop apps access your microphone in \
     Settings › Privacy & security › Microphone, then reconnect.";
/// The settings page [`PRIVACY_GUIDANCE`] links to.
pub const PRIVACY_SETTINGS_URI: &str = "ms-settings:privacy-microphone";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// No callback for this long ends capture (`ProgressStarved`), as on macOS.
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(2);
const SUPERVISE_INTERVAL: Duration = Duration::from_millis(50);
const PRIVACY_RECHECK: Duration = Duration::from_secs(1);
const CONSENT_STORE: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
/// HRESULT `E_ACCESSDENIED` as cpal prints it inside a backend error.
const ACCESS_DENIED_HRESULT: &str = "0x80070005";

/// What the Privacy & security › Microphone switches say for this app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophonePrivacy {
    /// No switch denies it (or none is set).
    Allowed,
    /// "Microphone access" is off for the whole device.
    DeniedForDevice,
    /// "Let apps access your microphone" is off for this user.
    DeniedForApps,
    /// "Let desktop apps access your microphone" is off.
    DeniedForDesktopApps,
    /// This app is switched off individually.
    DeniedForThisApp,
}

impl MicrophonePrivacy {
    /// Whether capture may start.
    #[must_use]
    pub const fn allows_capture(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// The decision from the switches' registry values (`Allow`/`Deny`).
#[must_use]
pub fn privacy_from_values(
    device: Option<&str>,
    apps: Option<&str>,
    desktop_apps: Option<&str>,
    this_app: Option<&str>,
) -> MicrophonePrivacy {
    let denied =
        |value: Option<&str>| value.is_some_and(|value| value.eq_ignore_ascii_case("Deny"));
    if denied(device) {
        MicrophonePrivacy::DeniedForDevice
    } else if denied(apps) {
        MicrophonePrivacy::DeniedForApps
    } else if denied(desktop_apps) {
        MicrophonePrivacy::DeniedForDesktopApps
    } else if denied(this_app) {
        MicrophonePrivacy::DeniedForThisApp
    } else {
        MicrophonePrivacy::Allowed
    }
}

/// The key Windows keeps a desktop app's own switch under: its path with
/// `\` written as `#`.
#[must_use]
pub fn non_packaged_app_key(executable: &str) -> String {
    format!(
        r"{CONSENT_STORE}\NonPackaged\{}",
        executable.replace('\\', "#")
    )
}

/// Reads the switches for this process.
#[must_use]
pub fn microphone_privacy() -> MicrophonePrivacy {
    let this_app = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(non_packaged_app_key))
        .and_then(|key| read_string(Hive::CurrentUser, &key, "Value"));
    privacy_from_values(
        read_string(Hive::LocalMachine, CONSENT_STORE, "Value").as_deref(),
        read_string(Hive::CurrentUser, CONSENT_STORE, "Value").as_deref(),
        read_string(
            Hive::CurrentUser,
            &format!(r"{CONSENT_STORE}\NonPackaged"),
            "Value",
        )
        .as_deref(),
        this_app.as_deref(),
    )
}

/// Opens Settings › Privacy & security › Microphone.
#[must_use]
pub fn open_privacy_settings() -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    let uri = HSTRING::from(PRIVACY_SETTINGS_URI);
    // SAFETY: static verb and a NUL-terminated URI; no window, no
    // parameters.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(uri.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    result.0 as isize > 32
}

/// How a cpal stream build failure reads as a capture error.
#[must_use]
pub fn classify_build_error(error: &cpal::BuildStreamError) -> MicrophoneCaptureError {
    match error {
        cpal::BuildStreamError::DeviceNotAvailable => MicrophoneCaptureError::InputUnavailable,
        cpal::BuildStreamError::BackendSpecific { err }
            if err.description.contains(ACCESS_DENIED_HRESULT) =>
        {
            MicrophoneCaptureError::PermissionDenied
        }
        _ => MicrophoneCaptureError::EngineStartFailed,
    }
}

/// A running WASAPI capture.
pub struct WindowsMicrophoneStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    captured: Arc<AtomicU64>,
}

impl std::fmt::Debug for WindowsMicrophoneStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsMicrophoneStream")
            .field("captured_frames", &self.captured.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl WindowsMicrophoneStream {
    /// Frames delivered to the sink so far.
    #[must_use]
    pub fn captured_frames(&self) -> u64 {
        self.captured.load(Ordering::Relaxed)
    }
}

impl MicrophoneStream for WindowsMicrophoneStream {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl Drop for WindowsMicrophoneStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = std::thread::Builder::new()
                .name("arcen-microphone-reaper".to_owned())
                .spawn(move || {
                    let _ = thread.join();
                });
        }
    }
}

/// Starts capture into `sink` (see `DeckServices::start_microphone`).
///
/// # Errors
///
/// Why capture did not start; `PermissionDenied` when Windows' privacy
/// switches refuse it.
pub fn start(
    sink: Arc<dyn MicrophoneFrameSink>,
    cancel: &MicrophoneCancel,
) -> Result<WindowsMicrophoneStream, MicrophoneCaptureError> {
    start_from(CaptureSource::DefaultInput, sink, cancel)
}

/// Where capture reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// The default recording device (the product).
    DefaultInput,
    /// WASAPI loopback of the default playback device. Lab only: proves the
    /// whole capture path on a machine whose only microphone is a virtual
    /// device that delivers nothing (a Teradici/RDP endpoint without a
    /// client), by capturing a tone played on that machine.
    RenderLoopback,
}

/// [`start`] from a chosen [`CaptureSource`].
///
/// # Errors
///
/// As [`start`].
pub fn start_from(
    source: CaptureSource,
    sink: Arc<dyn MicrophoneFrameSink>,
    cancel: &MicrophoneCancel,
) -> Result<WindowsMicrophoneStream, MicrophoneCaptureError> {
    if cancel.is_cancelled() {
        return Err(MicrophoneCaptureError::Cancelled);
    }
    if !microphone_privacy().allows_capture() {
        sink.finish(Err(MicrophoneCaptureError::PermissionDenied));
        return Err(MicrophoneCaptureError::PermissionDenied);
    }
    // The engine stops this capture through `cancel.stop`.
    let stop = Arc::clone(&cancel.stop);
    let captured = Arc::new(AtomicU64::new(0));
    let (startup, ready) = std::sync::mpsc::sync_channel(1);
    let thread_cancel = MicrophoneCancel {
        stop: Arc::clone(&stop),
        session_closed: Arc::clone(&cancel.session_closed),
    };
    let thread_captured = Arc::clone(&captured);
    let thread = std::thread::Builder::new()
        .name("arcen-wasapi-input".to_owned())
        .spawn(move || run(source, &sink, &thread_cancel, &thread_captured, &startup))
        .map_err(|_| MicrophoneCaptureError::InputUnavailable)?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if cancel.is_cancelled() {
            stop.store(true, Ordering::Release);
            let _ = thread.join();
            return Err(MicrophoneCaptureError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stop.store(true, Ordering::Release);
            let _ = thread.join();
            return Err(MicrophoneCaptureError::StartupTimedOut);
        }
        match ready.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(Ok(())) => {
                return Ok(WindowsMicrophoneStream {
                    stop,
                    thread: Some(thread),
                    captured,
                });
            }
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                return Err(error);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                return Err(MicrophoneCaptureError::InputUnavailable);
            }
        }
    }
}

/// What the device callback shares with the supervising thread.
struct CallbackState {
    normalizer: Mutex<MicrophoneCaptureNormalizer>,
    invalid: AtomicBool,
    device_lost: AtomicBool,
    stopped: AtomicBool,
    progress_ms: AtomicU64,
}

/// The device callback: normalise, push into the sink, note progress.
struct Deliver {
    state: Arc<CallbackState>,
    sink: Arc<dyn MicrophoneFrameSink>,
    cancel: MicrophoneCancel,
    captured: Arc<AtomicU64>,
    clock: Instant,
    channels: usize,
}

type Emit<'a> = dyn FnMut(NormalizedMicrophoneFrame) -> ControlFlow<()> + 'a;

impl Deliver {
    #[allow(clippy::mut_mut, reason = "a sized handle to the unsized emitter")]
    fn deliver(
        &self,
        feed: impl FnOnce(
            &mut MicrophoneCaptureNormalizer,
            usize,
            &mut &mut Emit<'_>,
        ) -> ControlFlow<()>,
    ) {
        super::join_pro_audio_once();
        if self.cancel.is_cancelled() {
            return;
        }
        let Ok(mut normalizer) = self.state.normalizer.lock() else {
            self.state.invalid.store(true, Ordering::Release);
            return;
        };
        let mut push = |mut frame: NormalizedMicrophoneFrame| {
            if self.cancel.is_cancelled() {
                return ControlFlow::Break(());
            }
            self.sink.push(CapturedMicrophoneFrame {
                samples: frame.take_samples(),
                sequence: frame.sequence,
                timestamp_ms: frame.timestamp_ms,
            });
            self.captured.fetch_add(1, Ordering::Relaxed);
            ControlFlow::Continue(())
        };
        let mut emit: &mut Emit<'_> = &mut push;
        let _ = feed(&mut normalizer, self.channels, &mut emit);
        self.state.progress_ms.store(
            u64::try_from(self.clock.elapsed().as_millis())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
            Ordering::Release,
        );
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one capture's life, start to finish, as the macOS run"
)]
fn run(
    source: CaptureSource,
    sink: &Arc<dyn MicrophoneFrameSink>,
    cancel: &MicrophoneCancel,
    captured: &Arc<AtomicU64>,
    startup: &std::sync::mpsc::SyncSender<Result<(), MicrophoneCaptureError>>,
) {
    let fail = |error: MicrophoneCaptureError| {
        sink.finish(Err(error));
        let _ = startup.send(Err(error));
    };
    let host = cpal::default_host();
    let device = match source {
        CaptureSource::DefaultInput => host.default_input_device(),
        CaptureSource::RenderLoopback => host.default_output_device(),
    };
    let Some(device) = device else {
        fail(MicrophoneCaptureError::InputUnavailable);
        return;
    };
    let supported = match source {
        CaptureSource::DefaultInput => device.default_input_config(),
        CaptureSource::RenderLoopback => device.default_output_config(),
    };
    let Ok(supported) = supported else {
        fail(MicrophoneCaptureError::InputUnavailable);
        return;
    };
    let channels = usize::from(supported.channels());
    let rate = supported.sample_rate().0;
    let Some(normalizer) = MicrophoneCaptureNormalizer::new(rate).filter(|_| channels > 0) else {
        fail(MicrophoneCaptureError::InvalidCallbackFormat);
        return;
    };
    if cancel.is_cancelled() {
        fail(MicrophoneCaptureError::Cancelled);
        return;
    }
    let state = Arc::new(CallbackState {
        normalizer: Mutex::new(normalizer),
        invalid: AtomicBool::new(false),
        device_lost: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        progress_ms: AtomicU64::new(0),
    });
    let clock = Instant::now();
    let config = cpal::StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let error_state = Arc::clone(&state);
    let on_error = move |error: cpal::StreamError| {
        if matches!(error, cpal::StreamError::DeviceNotAvailable) {
            error_state.device_lost.store(true, Ordering::Release);
        } else {
            error_state.stopped.store(true, Ordering::Release);
        }
    };
    let deliver = Deliver {
        state: Arc::clone(&state),
        sink: Arc::clone(sink),
        cancel: cancel.clone(),
        captured: Arc::clone(captured),
        clock,
        channels,
    };
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                deliver.deliver(|normalizer, channels, emit| {
                    normalizer.push_interleaved_f32(data, channels, emit)
                });
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                deliver.deliver(|normalizer, channels, emit| {
                    normalizer.push_interleaved_i16(data, channels, emit)
                });
            },
            on_error,
            None,
        ),
        _ => {
            fail(MicrophoneCaptureError::InvalidCallbackFormat);
            return;
        }
    };
    let stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            let classified = classify_build_error(&error);
            tracing::warn!(
                target: arcen_deck_core::log_target::AUDIO,
                %error,
                reason = ?classified,
                "microphone stream could not be opened"
            );
            fail(classified);
            return;
        }
    };
    if cancel.is_cancelled() {
        fail(MicrophoneCaptureError::Cancelled);
        return;
    }
    if let Err(error) = stream.play() {
        let classified = if error.to_string().contains(ACCESS_DENIED_HRESULT) {
            MicrophoneCaptureError::PermissionDenied
        } else {
            MicrophoneCaptureError::EngineStartFailed
        };
        fail(classified);
        return;
    }
    let _ = startup.send(Ok(()));

    let started = Instant::now();
    let mut privacy_checked = Instant::now();
    let failure = loop {
        if cancel.is_cancelled() {
            break None;
        }
        if state.device_lost.load(Ordering::Acquire) {
            break Some(MicrophoneCaptureError::InputRemoved);
        }
        if state.stopped.load(Ordering::Acquire) {
            break Some(MicrophoneCaptureError::EngineStopped);
        }
        if state.invalid.load(Ordering::Acquire) {
            break Some(MicrophoneCaptureError::InvalidCallbackFormat);
        }
        if privacy_checked.elapsed() >= PRIVACY_RECHECK {
            privacy_checked = Instant::now();
            if !microphone_privacy().allows_capture() {
                break Some(MicrophoneCaptureError::PermissionLost);
            }
        }
        let progress = state.progress_ms.load(Ordering::Acquire);
        let silent_for = u128::from(progress.saturating_sub(1));
        if started.elapsed() > PROGRESS_TIMEOUT
            && (progress == 0
                || clock.elapsed().as_millis().saturating_sub(silent_for)
                    > PROGRESS_TIMEOUT.as_millis())
        {
            break Some(MicrophoneCaptureError::ProgressStarved);
        }
        std::thread::park_timeout(SUPERVISE_INTERVAL);
    };
    drop(stream);
    state
        .normalizer
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    sink.finish(failure.map_or(Ok(()), Err));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn any_denying_switch_refuses_capture_and_names_itself() {
        assert_eq!(
            privacy_from_values(None, None, None, None),
            MicrophonePrivacy::Allowed
        );
        assert_eq!(
            privacy_from_values(Some("Allow"), Some("Allow"), Some("Allow"), None),
            MicrophonePrivacy::Allowed
        );
        assert_eq!(
            privacy_from_values(Some("Deny"), Some("Allow"), None, None),
            MicrophonePrivacy::DeniedForDevice
        );
        assert_eq!(
            privacy_from_values(None, Some("deny"), None, None),
            MicrophonePrivacy::DeniedForApps
        );
        assert_eq!(
            privacy_from_values(None, None, Some("Deny"), None),
            MicrophonePrivacy::DeniedForDesktopApps
        );
        assert_eq!(
            privacy_from_values(None, None, Some("Allow"), Some("Deny")),
            MicrophonePrivacy::DeniedForThisApp
        );
        assert!(!MicrophonePrivacy::DeniedForApps.allows_capture());
    }

    #[test]
    fn a_desktop_app_switch_lives_under_its_path() {
        assert!(
            non_packaged_app_key(r"C:\Program Files\Arcen\arcen-deck.exe")
                .ends_with(r"\NonPackaged\C:#Program Files#Arcen#arcen-deck.exe")
        );
    }

    #[test]
    fn an_access_denied_audio_client_is_a_refusal() {
        let denied = cpal::BuildStreamError::BackendSpecific {
            err: cpal::BackendSpecificError {
                description: "Access is denied. (0x80070005)".to_owned(),
            },
        };
        assert_eq!(
            classify_build_error(&denied),
            MicrophoneCaptureError::PermissionDenied
        );
        assert_eq!(
            classify_build_error(&cpal::BuildStreamError::DeviceNotAvailable),
            MicrophoneCaptureError::InputUnavailable
        );
        assert_eq!(
            classify_build_error(&cpal::BuildStreamError::StreamConfigNotSupported),
            MicrophoneCaptureError::EngineStartFailed
        );
    }

    #[test]
    fn a_cancelled_request_never_opens_a_device() {
        struct Sink;
        impl MicrophoneFrameSink for Sink {
            fn push(&self, _frame: CapturedMicrophoneFrame) {}
            fn finish(&self, _result: Result<(), MicrophoneCaptureError>) {}
        }
        let cancel = MicrophoneCancel {
            stop: Arc::new(AtomicBool::new(true)),
            session_closed: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(
            start(Arc::new(Sink), &cancel).map(|_| ()),
            Err(MicrophoneCaptureError::Cancelled)
        );
    }
}
