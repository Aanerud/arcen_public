//! Capturing the desktop's audio, and silencing its speakers.
//!
//! Two requirements meet here, and they are not the same requirement.
//!
//! The first is capture: a remote user should hear what the machine is
//! playing. The second is privacy: someone standing next to the host must not
//! hear it too. A remote session that quietly plays a call out of a speaker in
//! an empty office is a failure whether or not the audio also reached the
//! Deck, so local playback is silenced for the whole session by default —
//! including when audio redirection is switched off entirely.
//!
//! macOS has a public answer to both at once. A Core Audio process tap
//! (macOS 14.2 and later) can carry a copy of the system's output to this
//! process while muting the output itself. That is strictly better than the
//! obvious alternative of turning the system volume down, which changes a
//! setting the operator can see and fight with, is not scoped to the session,
//! and does not survive the machine being adjusted mid-call.
//!
//! The tap is created `private`, so it belongs to this process rather than
//! appearing as a device other applications can select, and it is destroyed on
//! `Drop` — which is what restores local sound. Restoration is not left to
//! chance: [`SystemAudioTap::release`] reports whether it actually happened,
//! because a mute that cannot be lifted leaves a machine silent after the
//! session has gone.
//!
//! This module owns the native half only. Whether a session may carry audio at
//! all, and in what form, is [`arcen_media`]'s and the host configuration's
//! decision.

use arcen_session::pier_config::LocalPlayback;

/// Why audio could not be established or released.
#[derive(Debug)]
pub enum AudioError {
    /// This macOS is older than the process-tap API.
    Unsupported(String),
    /// The tap could not be created.
    ///
    /// Carries the native `OSStatus`, which distinguishes "not permitted" from
    /// "no such device" — different problems with different fixes.
    TapFailed { status: i32, detail: String },
    /// The tap was created but its audio format could not be read.
    ///
    /// Treated as a failure rather than guessed at: capturing into an assumed
    /// format produces noise, and noise is harder to diagnose than nothing.
    FormatUnavailable { status: i32 },
    /// The tap could not be destroyed, so local audio may still be muted.
    ReleaseFailed { status: i32 },
    /// The tap exists but audio could not actually be captured through it.
    CaptureFailed { status: i32, detail: String },
    /// The tap's real format is not one this adapter can carry honestly.
    UnsupportedFormat { detail: String },
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(detail) => write!(formatter, "audio tap unsupported: {detail}"),
            Self::TapFailed { status, detail } => {
                write!(formatter, "audio tap failed ({status}): {detail}")
            }
            Self::FormatUnavailable { status } => {
                write!(formatter, "audio tap format unavailable ({status})")
            }
            Self::ReleaseFailed { status } => write!(
                formatter,
                "audio tap could not be released ({status}); local audio may still be muted"
            ),
            Self::CaptureFailed { status, detail } => {
                write!(formatter, "audio capture failed ({status}): {detail}")
            }
            Self::UnsupportedFormat { detail } => {
                write!(formatter, "audio format unsupported: {detail}")
            }
        }
    }
}

impl std::error::Error for AudioError {}

/// The audio format a tap actually produces.
///
/// Read back from the tap rather than assumed. The shared audio contract wants
/// 48 kHz stereo; whether this machine's current output device provides that is
/// a fact to be discovered, not a setting to be declared.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct TapFormat {
    /// Sample rate in hertz.
    pub sample_rate_hz: f64,
    /// Channels per frame.
    pub channels: u32,
    /// Bits per sample.
    pub bits_per_channel: u32,
    /// Whether samples are floating point.
    pub float: bool,
}

/// What a mute lease did, for the record.
///
/// Three separate facts, because collapsing them produces a field that reads
/// as "mute was applied" while reporting that it deliberately was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct MuteEvidence {
    /// Whether the host configuration asked for local playback to be silenced.
    pub requested: bool,
    /// What Core Audio reports the tapped output is actually doing.
    ///
    /// `None` when the property could not be read, which is not the same as
    /// audible and must not be treated as either answer.
    pub observed_muted: Option<bool>,
    /// Whether the observed state matches what was requested.
    ///
    /// This is the field a session decides on. A requested mute that is not
    /// honoured must refuse the session.
    pub honoured: bool,
}

#[cfg(target_os = "macos")]
mod native {
    // Native Core Audio calls, like the capture and encode adapters beside
    // this one. Every block below carries its own SAFETY note.
    #![allow(unsafe_code)]

    use super::{AudioError, MuteEvidence, TapFormat};
    use arcen_media::audio::{AUDIO_V1_SAMPLE_RATE_HZ, AudioFrameSpec, PcmPacketizer};
    use arcen_session::pier_config::LocalPlayback;
    use block2::RcBlock;
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_audio::{
        AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
        AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
        AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
        AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectID,
        AudioObjectPropertyAddress, CATapDescription, CATapMuteBehavior,
        kAudioAggregateDeviceClockDeviceKey, kAudioAggregateDeviceIsPrivateKey,
        kAudioAggregateDeviceIsStackedKey, kAudioAggregateDeviceMainSubDeviceKey,
        kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
        kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
        kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSizeRange,
        kAudioDevicePropertyDeviceUID, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
        kAudioSubDeviceDriftCompensationKey, kAudioSubDeviceUIDKey,
        kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyDescription,
        kAudioTapPropertyFormat, kAudioTapPropertyUID,
    };
    use objc2_core_audio_types::{
        AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioValueRange,
    };
    use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
    use std::ffi::CStr;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// `kAudioFormatFlagIsFloat`, which the types crate does not re-export.
    const FORMAT_FLAG_IS_FLOAT: u32 = 1;

    /// A live system-output tap.
    ///
    /// Holding one of these is what keeps local playback muted, so its
    /// lifetime is the session's.
    #[derive(Debug)]
    pub struct SystemAudioTap {
        tap: AudioObjectID,
        mute: MuteEvidence,
        released: bool,
        /// Failed destroy attempts. Non-zero means local audio may still be
        /// muted and this process could not undo it.
        release_failures: u32,
    }

    // SAFETY: the tap id is an opaque Core Audio object owned by this process,
    // and this type never exposes references into Core Audio memory. Moving
    // the lease only changes which Rust thread is responsible for eventually
    // calling `AudioHardwareDestroyProcessTap`, which is a HAL teardown call
    // rather than thread-local state.
    unsafe impl Send for SystemAudioTap {}

    impl SystemAudioTap {
        /// Creates a private tap over the whole system output.
        ///
        /// # Errors
        ///
        /// Returns [`AudioError`] when the tap cannot be created.
        pub fn create(local_playback: LocalPlayback) -> Result<Self, AudioError> {
            let requested_mute = local_playback.requires_mute();

            // An empty exclusion list is what makes this a *global* tap: every
            // process is included because none is excluded. Naming processes
            // instead would silently miss anything launched later in the
            // session, which is the common case — a user opens a video after
            // connecting, not before.
            let exclude: Retained<NSArray<NSNumber>> = NSArray::new();

            // SAFETY: `CATapDescription` is an Objective-C class from
            // CoreAudio. `alloc` produces an owned uninitialised allocation,
            // and the initialiser consumes it exactly once, which is the
            // ownership contract objc2 encodes in `Allocated`. `exclude` is a
            // live `NSArray` for the duration of the call.
            let description = unsafe {
                let allocated = CATapDescription::alloc();
                CATapDescription::initStereoGlobalTapButExcludeProcesses(allocated, &exclude)
            };

            // SAFETY: `description` is a live, initialised `CATapDescription`.
            // These are ordinary property setters with no further contract.
            unsafe {
                // Private: the tap belongs to this process rather than
                // appearing as a device other applications can select.
                description.setPrivate(true);
                description.setMuteBehavior(if requested_mute {
                    // Mutes the captured output while still delivering it
                    // here. This is the behaviour the whole design rests on.
                    CATapMuteBehavior::Muted
                } else {
                    CATapMuteBehavior::Unmuted
                });
                description.setName(&NSString::from_str("Arcen Pier session audio"));
            }

            let mut tap: AudioObjectID = 0;
            // SAFETY: `description` is live, and `tap` is a valid, writable
            // `AudioObjectID` that the call fills in on success. The function
            // reports failure through its return value rather than by writing
            // an invalid id, so `tap` is only trusted below when the status is
            // zero.
            let status = unsafe { AudioHardwareCreateProcessTap(Some(&description), &raw mut tap) };
            if status != 0 {
                return Err(AudioError::TapFailed {
                    status,
                    detail: describe_status(status),
                });
            }

            let mut session = Self {
                tap,
                mute: MuteEvidence {
                    requested: requested_mute,
                    // Filled in from the tap itself below. Assuming it worked
                    // because it was asked for is exactly the kind of evidence
                    // that reports a feature as working when it is not.
                    observed_muted: None,
                    honoured: false,
                },
                released: false,
                release_failures: 0,
            };
            let observed = session.readback_mute_behavior();
            session.mute.observed_muted = observed;
            session.mute.honoured = observed == Some(requested_mute);

            Ok(session)
        }

        /// Returns the tap's unique identifier.
        ///
        /// The aggregate device refers to the tap by this string rather than
        /// by object id, so capture cannot start without it.
        ///
        /// # Errors
        ///
        /// Returns [`AudioError::CaptureFailed`] when the property cannot be
        /// read.
        pub fn uid(&self) -> Result<String, AudioError> {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioTapPropertyUID,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut raw: *const CFString = std::ptr::null();
            let mut size = u32::try_from(size_of::<*const CFString>()).unwrap_or(0);

            // SAFETY: `self.tap` is a live tap this type created. The pointers
            // address live locals, and `size` states the exact size of the
            // one-pointer buffer offered, so Core Audio cannot overrun it.
            let status = unsafe {
                AudioObjectGetPropertyData(
                    self.tap,
                    NonNull::from(&address),
                    0,
                    std::ptr::null(),
                    NonNull::from(&mut size),
                    NonNull::from(&mut raw).cast(),
                )
            };
            if status != 0 || raw.is_null() {
                return Err(AudioError::CaptureFailed {
                    status,
                    detail: "tap has no readable UID".to_owned(),
                });
            }

            // SAFETY: this property follows Core Foundation's create rule, so
            // ownership of the string is taken here and released when the
            // `CFRetained` drops. `raw` was checked non-null above.
            let uid = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw.cast_mut())) };
            Ok(uid.to_string())
        }

        /// Reads the mute behaviour back from the live tap.
        ///
        /// Returns `Some(true)` when Core Audio reports the tapped output is
        /// muted, `Some(false)` when it reports it is audible, and `None` when
        /// the property cannot be read. The point is to record what the system
        /// says, not what this process asked for.
        fn readback_mute_behavior(&self) -> Option<bool> {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioTapPropertyDescription,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut raw: *mut CATapDescription = std::ptr::null_mut();
            let mut size = u32::try_from(size_of::<*mut CATapDescription>()).unwrap_or(0);

            // SAFETY: `self.tap` is a live tap this type created. The pointers
            // come from live locals, and `size` states the exact size of the
            // one-pointer buffer being offered, so Core Audio cannot overrun
            // it. On success it writes a single object pointer into `raw`.
            let status = unsafe {
                AudioObjectGetPropertyData(
                    self.tap,
                    NonNull::from(&address),
                    0,
                    std::ptr::null(),
                    NonNull::from(&mut size),
                    NonNull::from(&mut raw).cast(),
                )
            };
            if status != 0 {
                return None;
            }

            // SAFETY: this property is adopted under Core Foundation's create
            // rule. That is not stated in `AudioHardware.h` — the header
            // documents caller-release explicitly for the sibling
            // `kAudioTapPropertyUID` on the same object but says nothing here —
            // so it was checked empirically instead: 40 create/read/release
            // cycles under `MallocScribble` and `MallocPreScribble`, which turn
            // an over-release into a likely crash, completed cleanly. That is
            // evidence rather than proof. If Apple documents this as borrowed,
            // switch to retaining it; the observable symptom of getting it
            // wrong in that direction is a use-after-free here.
            // `from_raw` returns `None` for a null pointer, which covers a call
            // that reported success without producing an object.
            let description = unsafe { Retained::from_raw(raw) }?;

            // SAFETY: `description` is a live `CATapDescription`; this is a
            // plain property read.
            let behavior = unsafe { description.isMuted() };
            Some(behavior == CATapMuteBehavior::Muted)
        }

        /// Returns the format this tap actually produces.
        ///
        /// # Errors
        ///
        /// Returns [`AudioError::FormatUnavailable`] when Core Audio refuses
        /// the property.
        pub fn format(&self) -> Result<TapFormat, AudioError> {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioTapPropertyFormat,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            // Every field is a plain integer or float, so an all-zero pattern
            // is a valid value rather than a lie about initialisation. Core
            // Audio overwrites it wholesale on success, and the status is
            // checked before anything is read back.
            let mut description = AudioStreamBasicDescription {
                mSampleRate: 0.0,
                mFormatID: 0,
                mFormatFlags: 0,
                mBytesPerPacket: 0,
                mFramesPerPacket: 0,
                mBytesPerFrame: 0,
                mChannelsPerFrame: 0,
                mBitsPerChannel: 0,
                mReserved: 0,
            };
            let mut size = u32::try_from(size_of::<AudioStreamBasicDescription>()).unwrap_or(0);

            // SAFETY: `self.tap` is an id this type created and has not
            // released. The three pointers are taken from live locals on this
            // stack frame, so none is null and all outlive the call. `size` is
            // initialised to exactly the size of `description`, which is the
            // contract for this call, so Core Audio cannot write past it.
            let status = unsafe {
                AudioObjectGetPropertyData(
                    self.tap,
                    NonNull::from(&address),
                    0,
                    std::ptr::null(),
                    NonNull::from(&mut size),
                    NonNull::from(&mut description).cast(),
                )
            };
            if status != 0 {
                return Err(AudioError::FormatUnavailable { status });
            }

            Ok(TapFormat {
                sample_rate_hz: description.mSampleRate,
                channels: description.mChannelsPerFrame,
                bits_per_channel: description.mBitsPerChannel,
                float: description.mFormatFlags & FORMAT_FLAG_IS_FLOAT != 0,
            })
        }

        /// Returns what this tap did about local playback.
        #[must_use]
        pub const fn mute_evidence(&self) -> MuteEvidence {
            self.mute
        }

        /// Releases the tap, restoring local playback.
        ///
        /// Reported rather than silent: a mute that cannot be lifted leaves a
        /// machine with no sound after the session has ended, and whoever is
        /// sitting at it has no way to know why.
        ///
        /// # Errors
        ///
        /// Returns [`AudioError::ReleaseFailed`] when the tap survives.
        pub fn release(&mut self) -> Result<(), AudioError> {
            if self.released {
                return Ok(());
            }
            // SAFETY: `self.tap` was produced by `AudioHardwareCreateProcessTap`
            // and has not been destroyed, which `released` tracks.
            let status = unsafe { AudioHardwareDestroyProcessTap(self.tap) };
            if status == 0 {
                self.released = true;
                // The tap is gone, so the output it was muting is audible
                // again.
                self.mute.observed_muted = Some(false);
                return Ok(());
            }

            // The tap survived. Do NOT record it as released: that would make
            // every later attempt return success while the machine stays
            // silent and this process has forgotten it owes anyone a fix.
            // What is actually known is that the state is now unknown.
            self.mute.observed_muted = None;
            self.release_failures += 1;
            tracing::error!(
                target: "arcen::audio",
                status,
                attempts = self.release_failures,
                "audio tap release failed; local playback may still be muted"
            );
            Err(AudioError::ReleaseFailed { status })
        }
    }

    impl Drop for SystemAudioTap {
        fn drop(&mut self) {
            // A session that ends abruptly must still give the machine its
            // sound back. One retry, because the common transient cause is
            // the HAL being busy, and then a record an operator can find:
            // silently leaving a Mac muted is the worst outcome this module
            // can produce.
            if self.release().is_err() && self.release().is_err() {
                tracing::error!(
                    target: "arcen::audio",
                    attempts = self.release_failures,
                    "audio tap could not be released; this Mac may have no sound until it is \
                     restarted or the tap is cleared"
                );
            }
        }
    }

    /// The block Core Audio calls for every buffer of audio.
    type IoBlock = RcBlock<
        dyn Fn(
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
        ),
    >;

    /// A running capture: the tap plus the aggregate device that reads it.
    ///
    /// A tap on its own is a route, not a stream. Audio only arrives once an
    /// aggregate device is built around the tap and an IO proc is started on
    /// it, which is why proving the tap exists is not the same as proving this
    /// host can capture sound.
    #[derive(Debug)]
    pub struct AudioCaptureSession {
        tap: SystemAudioTap,
        aggregate: AudioObjectID,
        proc_id: AudioDeviceIOProcID,
        // The block is retained for exactly as long as the IO proc that calls
        // it. Dropping it earlier would leave Core Audio calling freed memory
        // from its own real-time thread.
        _block: IoBlock,
        observed: Arc<CaptureCounters>,
        /// Samples the callback has handed over, waiting to be framed.
        pending: Arc<Mutex<Vec<f32>>>,
        /// Swapped with `pending` so draining never gives the callback a fresh vector.
        drain_buffer: Vec<f32>,
        /// Frames them. Owned here, not by the callback, because it allocates.
        packetizer: PcmPacketizer,
        running: bool,
    }

    /// A recorder failure after the mute lease has already been established.
    #[derive(Debug)]
    pub struct AudioRecorderStartupError {
        /// The live tap that still owns the local-playback policy.
        pub tap: SystemAudioTap,
        /// Why sample capture could not be started.
        pub source: AudioError,
    }

    // SAFETY: the only field that is not already `Send` is the retained IO
    // block, and moving this value does not move who calls it. Core Audio
    // invokes that block from its own real-time thread for the whole life of
    // the IO proc, on whichever thread created it and independently of which
    // thread owns this struct; Rust code never dereferences the block pointer,
    // it only retains it so the callback is not called into freed memory.
    //
    // Everything the callback touches is already shareable: `observed` is
    // atomics and `pending` is behind a mutex, both in `Arc`. The remaining
    // fields are a plain `AudioObjectID`, an opaque proc id, a data-only
    // packetizer, and a bool.
    //
    // Concurrent use is prevented by the API, not by this impl: every method
    // that reads or drains takes `&mut self`, so two threads cannot be inside
    // one session at once. `Sync` is deliberately not asserted, because a
    // shared reference would allow exactly that.
    //
    // Teardown is thread-agnostic: `AudioDeviceStop`,
    // `AudioDeviceDestroyIOProcID` and the CF releases in `stop`/`Drop` are
    // safe to call from any thread.
    unsafe impl Send for AudioCaptureSession {}

    /// What the IO callback has seen.
    ///
    /// Atomics because this is written from Core Audio's real-time thread. A
    /// mutex here would let a slow reader stall audio for the whole machine,
    /// which is the one thing an audio callback must never do — along with
    /// allocating, logging, or touching a file.
    #[derive(Debug, Default)]
    pub struct CaptureCounters {
        /// Times the IO proc was called.
        pub callbacks: AtomicU64,
        /// Total audio frames delivered.
        pub frames: AtomicU64,
        /// Peak absolute sample value, scaled by 1e6 so it fits an integer.
        ///
        /// This is what distinguishes "audio is flowing" from "a callback is
        /// running and handing us silence" — the difference between capture
        /// working and capture being denied.
        pub peak_micro: AtomicU64,
        /// Complete audio-v1 packets produced from the captured samples.
        pub packets: AtomicU64,
        /// Buffers in the most recent callback.
        ///
        /// One buffer carrying two channels is interleaved; two buffers of one
        /// channel each is planar. The packetizer needs interleaved input, so
        /// which one arrives is a fact worth recording rather than assuming.
        pub buffers_last: AtomicU64,
        /// Channels in the first buffer of the most recent callback.
        pub channels_last: AtomicU64,
        /// Callbacks whose buffer layout this adapter could not interleave.
        pub unsupported_layout: AtomicU64,
        /// Callbacks that could not take a lock, so their audio was skipped.
        pub contended_buffers: AtomicU64,
        /// Callbacks dropped because the pending queue was full.
        pub dropped_buffers: AtomicU64,
    }

    /// How many interleaved samples may wait to be framed.
    ///
    /// Two seconds of audio-v1. Enough to ride out a scheduling hiccup in the
    /// consumer, small enough that a stalled one costs bounded memory rather
    /// than the machine.
    const MAX_PENDING_SAMPLES: usize = AUDIO_V1_SAMPLE_RATE_HZ as usize * 2 * 2;

    impl AudioCaptureSession {
        /// Starts capturing system output.
        ///
        /// # Errors
        ///
        /// Returns [`AudioError`] when the tap, the aggregate device, or the
        /// IO proc cannot be established.
        #[allow(clippy::too_many_lines)]
        pub fn start(local_playback: LocalPlayback) -> Result<Self, AudioError> {
            tracing::info!(target: "arcen::media", stage = "tap", "audio start");
            let tap = SystemAudioTap::create(local_playback)?;
            Self::start_with_tap(tap).map_err(|error| error.source)
        }

        /// Starts recording through an already-established mute lease.
        ///
        /// # Errors
        ///
        /// Returns the original tap with the recorder error when capture cannot
        /// be started.
        #[allow(clippy::too_many_lines)]
        pub fn start_with_tap(tap: SystemAudioTap) -> Result<Self, AudioRecorderStartupError> {
            let recorder_error = |tap, source| AudioRecorderStartupError { tap, source };

            // The callback reads samples as `f32` and the packetizer frames
            // them as 48 kHz stereo. Both were previously assumed. A 44.1 kHz
            // source divided into 1,920-sample packets is not resampled — it
            // is played at the wrong speed — and reading integer samples as
            // floats is noise. Establish the format before starting, and
            // refuse what this adapter cannot honestly carry.
            tracing::info!(target: "arcen::media", stage = "format", "audio start");
            let format = match tap.format() {
                Ok(format) => format,
                Err(source) => return Err(recorder_error(tap, source)),
            };
            if let Err(error) = require_capture_format(format, AudioFrameSpec::V1) {
                return Err(recorder_error(tap, error));
            }

            tracing::info!(target: "arcen::media", stage = "uid", "audio start");
            let tap_uid = match tap.uid() {
                Ok(uid) => uid,
                Err(source) => return Err(recorder_error(tap, source)),
            };

            tracing::info!(target: "arcen::media", stage = "aggregate", "audio start");
            let aggregate = match create_aggregate_device(&tap_uid) {
                Ok(aggregate) => aggregate,
                Err(source) => return Err(recorder_error(tap, source)),
            };
            let callback_capacity = match callback_sample_capacity(aggregate, format) {
                Ok(capacity) => capacity,
                Err(error) => {
                    // SAFETY: `aggregate` was created above and not yet destroyed.
                    unsafe { AudioHardwareDestroyAggregateDevice(aggregate) };
                    return Err(recorder_error(tap, error));
                }
            };
            let observed = Arc::new(CaptureCounters::default());
            let counters = Arc::clone(&observed);

            // The wire's frame size, not the device's: the shared contract
            // owns where a packet ends. It lives on this side of the callback
            // because framing allocates.
            let Some(packetizer) = PcmPacketizer::new(AudioFrameSpec::V1) else {
                // SAFETY: `aggregate` was created above and not yet destroyed.
                unsafe { AudioHardwareDestroyAggregateDevice(aggregate) };
                return Err(recorder_error(
                    tap,
                    AudioError::CaptureFailed {
                        status: 0,
                        detail: "audio-v1 frame specification is invalid".to_owned(),
                    },
                ));
            };
            // Preallocated so the real-time callback only copies into bounded buffers.
            let staging = Arc::new(Mutex::new(Vec::<f32>::with_capacity(callback_capacity)));
            let pending = Arc::new(Mutex::new(Vec::<f32>::with_capacity(MAX_PENDING_SAMPLES)));
            let drain_buffer = Vec::<f32>::with_capacity(MAX_PENDING_SAMPLES);
            let callback_pending = Arc::clone(&pending);

            let block = RcBlock::new(
                move |_now: NonNull<AudioTimeStamp>,
                      input: NonNull<AudioBufferList>,
                      _input_time: NonNull<AudioTimeStamp>,
                      _output: NonNull<AudioBufferList>,
                      _output_time: NonNull<AudioTimeStamp>| {
                    // SAFETY: Core Audio guarantees `input` points at a valid
                    // buffer list for the duration of this call, and this
                    // closure does not retain it. Only reads happen here.
                    unsafe { accumulate(input, &counters, &callback_pending, &staging) };
                },
            );

            let mut proc_id: AudioDeviceIOProcID = None;
            // SAFETY: `proc_id` is a live local the call fills in. The block
            // pointer is valid and is kept alive by `block` below for at least
            // as long as the proc exists. A null queue means Core Audio uses
            // its own real-time thread, which is what an audio callback wants.
            tracing::info!(target: "arcen::media", stage = "ioproc", "audio start");
            let status = unsafe {
                AudioDeviceCreateIOProcIDWithBlock(
                    NonNull::from(&mut proc_id),
                    aggregate,
                    None,
                    RcBlock::as_ptr(&block),
                )
            };
            if status != 0 {
                // SAFETY: `aggregate` was created above and not yet destroyed.
                unsafe { AudioHardwareDestroyAggregateDevice(aggregate) };
                return Err(recorder_error(
                    tap,
                    AudioError::CaptureFailed {
                        status,
                        detail: "could not attach an IO proc to the aggregate device".to_owned(),
                    },
                ));
            }

            // SAFETY: both ids were produced by the calls above and are live.
            tracing::info!(target: "arcen::media", stage = "device_start", "audio start");
            let status = unsafe { AudioDeviceStart(aggregate, proc_id) };
            if status != 0 {
                // SAFETY: the proc was created and the device exists; this is
                // the documented teardown order.
                unsafe {
                    AudioDeviceDestroyIOProcID(aggregate, proc_id);
                    AudioHardwareDestroyAggregateDevice(aggregate);
                }
                return Err(recorder_error(
                    tap,
                    AudioError::CaptureFailed {
                        status,
                        detail: describe_status(status),
                    },
                ));
            }

            Ok(Self {
                tap,
                aggregate,
                proc_id,
                _block: block,
                observed,
                pending,
                drain_buffer,
                packetizer,
                running: true,
            })
        }

        /// Whether Core Audio has called back at least once.
        ///
        /// A tap that creates, formats and starts is not a tap that delivers.
        /// Measured on a machine with the grant in place and sound playing:
        /// all five start stages completed and the callback never ran, so a
        /// host that treated "started" as "working" told the Deck audio was
        /// enabled and then sent nothing, and the Deck waited out its media
        /// timeout for packets that were never coming.
        #[must_use]
        pub fn has_delivered(&self) -> bool {
            self.observed.callbacks.load(Ordering::Relaxed) > 0
        }

        /// Frames whatever the callback has handed over.
        ///
        /// Called from an ordinary thread, never the audio callback, because
        /// framing allocates. Returns the completed audio-v1 packets, which
        /// are what a session puts on the wire.
        pub fn drain_packets(&mut self) -> Vec<Vec<i16>> {
            let mut packets = Vec::new();
            if !drain_pending_samples(&self.pending, &mut self.drain_buffer) {
                return packets;
            }
            if self.drain_buffer.is_empty() {
                return packets;
            }
            self.packetizer.push(&self.drain_buffer, &mut packets);
            self.drain_buffer.clear();
            if !packets.is_empty() {
                self.observed
                    .packets
                    .fetch_add(packets.len() as u64, Ordering::Relaxed);
            }
            packets
        }

        /// Returns what the callback has observed so far.
        #[must_use]
        pub fn counters(&self) -> &CaptureCounters {
            &self.observed
        }

        /// Returns the tap's mute evidence.
        #[must_use]
        pub const fn mute_evidence(&self) -> MuteEvidence {
            self.tap.mute_evidence()
        }

        /// Returns the format being captured.
        ///
        /// # Errors
        ///
        /// Returns [`AudioError`] when the tap format cannot be read.
        pub fn format(&self) -> Result<TapFormat, AudioError> {
            self.tap.format()
        }

        /// Stops capture and releases everything, restoring local playback.
        ///
        /// # Errors
        ///
        /// Returns [`AudioError`] when the tap survives teardown.
        pub fn stop(&mut self) -> Result<(), AudioError> {
            if self.running {
                self.running = false;
                // Stop before destroy: tearing down a proc that is still
                // running is how a callback ends up firing into freed state.
                // SAFETY: both ids are live and this is the documented order.
                unsafe {
                    AudioDeviceStop(self.aggregate, self.proc_id);
                    AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
                    AudioHardwareDestroyAggregateDevice(self.aggregate);
                }
            }
            self.tap.release()
        }
    }

    impl Drop for AudioCaptureSession {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    pub(super) fn require_capture_format(
        format: TapFormat,
        expected: AudioFrameSpec,
    ) -> Result<(), AudioError> {
        if !format.float || format.bits_per_channel != 32 {
            return Err(AudioError::UnsupportedFormat {
                detail: format!(
                    "tap delivers {}-bit {}, but this adapter reads 32-bit float",
                    format.bits_per_channel,
                    if format.float { "float" } else { "integer" }
                ),
            });
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate = format.sample_rate_hz.round() as u32;
        if rate != expected.sample_rate_hz {
            return Err(AudioError::UnsupportedFormat {
                detail: format!(
                    "tap delivers {rate} Hz, but audio-v1 is {} Hz and this adapter does \
                     not resample",
                    expected.sample_rate_hz
                ),
            });
        }
        if format.channels != u32::from(expected.channels) {
            return Err(AudioError::UnsupportedFormat {
                detail: format!(
                    "tap delivers {} channels, but audio-v1 is {}",
                    format.channels, expected.channels
                ),
            });
        }
        Ok(())
    }

    fn drain_pending_samples(pending: &Mutex<Vec<f32>>, drain_buffer: &mut Vec<f32>) -> bool {
        let Ok(mut queue) = pending.lock() else {
            return false;
        };
        std::mem::swap(&mut *queue, drain_buffer);
        true
    }

    /// Reads one buffer list into the counters.
    ///
    /// # Safety
    ///
    /// `list` must point at a valid `AudioBufferList` whose `mNumberBuffers`
    /// describes the buffers that follow it, as Core Audio provides in an IO
    /// callback. Each buffer's `mData`/`mDataByteSize` must describe readable
    /// memory for the duration of the call.
    unsafe fn accumulate(
        list: NonNull<AudioBufferList>,
        counters: &CaptureCounters,
        pending: &Mutex<Vec<f32>>,
        interleaved: &Mutex<Vec<f32>>,
    ) {
        counters.callbacks.fetch_add(1, Ordering::Relaxed);

        // SAFETY: the caller guarantees `list` is a valid buffer list.
        let header = unsafe { list.as_ref() };
        let count = header.mNumberBuffers as usize;
        if count == 0 {
            return;
        }

        // The buffers are laid out contiguously after the header; the declared
        // array length of one is a C convention, not the real count.
        // SAFETY: the caller guarantees `mNumberBuffers` describes the
        // buffers that follow, which is Core Audio's documented layout.
        let buffers = unsafe { std::slice::from_raw_parts(header.mBuffers.as_ptr(), count) };
        counters.buffers_last.store(count as u64, Ordering::Relaxed);
        counters
            .channels_last
            .store(u64::from(buffers[0].mNumberChannels), Ordering::Relaxed);

        // These locks are uncontended in practice: only this callback takes
        // them, and the reader clones counters rather than the buffers. A
        // contended lock here would stall audio for the whole machine, so the
        // callback never blocks on a poisoned one either.
        // `try_lock` rather than `lock`: an audio callback must never wait.
        // A contended or poisoned staging buffer is counted as its own thing,
        // because calling it an unsupported layout would send someone looking
        // for a format bug that is not there.
        let Ok(mut staging) = interleaved.try_lock() else {
            counters.contended_buffers.fetch_add(1, Ordering::Relaxed);
            return;
        };
        staging.clear();

        let mut peak = 0.0f32;
        let frames: u64;

        if count == 1 {
            // Interleaved already: one buffer carrying every channel.
            let buffer = &buffers[0];
            if buffer.mData.is_null() || buffer.mNumberChannels == 0 {
                return;
            }
            let channels = buffer.mNumberChannels as usize;
            let samples = buffer.mDataByteSize as usize / size_of::<f32>();
            let accepted_samples = samples.min(staging.capacity() / channels * channels);
            if accepted_samples < samples {
                counters.dropped_buffers.fetch_add(1, Ordering::Relaxed);
            }
            // SAFETY: the caller guarantees `mData` addresses `mDataByteSize`
            // readable bytes. `AudioCaptureSession::start` verified the tap's
            // format is 32-bit float before this callback could run, and the
            // length is clamped to the preallocated callback buffer.
            let data =
                unsafe { std::slice::from_raw_parts(buffer.mData.cast::<f32>(), accepted_samples) };
            for &sample in data {
                peak = peak.max(sample.abs());
            }
            staging.extend_from_slice(data);
            frames = (accepted_samples / channels) as u64;
        } else {
            // Planar: one mono buffer per channel, which must be interleaved
            // before it can be framed. Feeding planar audio to the packetizer
            // unchanged produces one channel played at double speed followed
            // by the other, which is audible as a stutter rather than as an
            // error.
            let per_channel = buffers
                .iter()
                .map(|buffer| {
                    if buffer.mData.is_null() {
                        0
                    } else {
                        buffer.mDataByteSize as usize / size_of::<f32>()
                    }
                })
                .min()
                .unwrap_or(0);
            if per_channel == 0 {
                counters.unsupported_layout.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let accepted_per_channel = per_channel.min(staging.capacity() / count);
            if accepted_per_channel == 0 {
                counters.dropped_buffers.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if accepted_per_channel < per_channel {
                counters.dropped_buffers.fetch_add(1, Ordering::Relaxed);
            }
            staging.resize(accepted_per_channel * count, 0.0);
            for (channel, buffer) in buffers.iter().enumerate() {
                // SAFETY: as above; `accepted_per_channel` is no larger than
                // the smallest declared length across the buffers.
                let data = unsafe {
                    std::slice::from_raw_parts(buffer.mData.cast::<f32>(), accepted_per_channel)
                };
                for (frame, &sample) in data.iter().enumerate() {
                    peak = peak.max(sample.abs());
                    staging[frame * count + channel] = sample;
                }
            }
            frames = accepted_per_channel as u64;
        }

        counters.frames.fetch_add(frames, Ordering::Relaxed);
        // Samples are nominally -1.0..=1.0, but a format change or a rogue
        // buffer could carry anything, including a NaN. Clamping first means a
        // bad sample cannot wrap the counter into a nonsensical peak.
        let bounded = if peak.is_finite() {
            peak.clamp(0.0, 1000.0)
        } else {
            0.0
        };
        // The clamp above is the proof: the value is non-negative and at most
        // 1e9 after scaling, which is far inside `u64`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scaled = (f64::from(bounded) * 1_000_000.0) as u64;
        counters.peak_micro.fetch_max(scaled, Ordering::Relaxed);

        // Framing deliberately does NOT happen here. `PcmPacketizer::push`
        // allocates, and an allocator stall on Core Audio's real-time thread
        // is heard by everyone using the machine, not just this session. The
        // samples go to a bounded queue and a worker frames them.
        //
        // The queue is bounded because the alternative is an unbounded one: a
        // stalled consumer would otherwise grow it until the machine runs out
        // of memory, and dropping audio is the better failure.
        if let Ok(mut queue) = pending.try_lock() {
            if queue.len() + staging.len() <= MAX_PENDING_SAMPLES {
                queue.extend_from_slice(&staging);
            } else {
                counters.dropped_buffers.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            counters.contended_buffers.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns the largest interleaved sample count Core Audio may deliver at once.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn callback_sample_capacity(
        aggregate: AudioObjectID,
        format: TapFormat,
    ) -> Result<usize, AudioError> {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyBufferFrameSizeRange,
            mScope: kAudioObjectPropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut range = AudioValueRange {
            mMinimum: 0.0,
            mMaximum: 0.0,
        };
        let mut size = u32::try_from(size_of::<AudioValueRange>()).unwrap_or(0);
        // SAFETY: `aggregate` was created by Core Audio and is live. The
        // pointers address live locals, and `size` states the exact buffer
        // offered for the single `AudioValueRange` result.
        let status = unsafe {
            AudioObjectGetPropertyData(
                aggregate,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut range).cast(),
            )
        };
        if status != 0 || !range.mMaximum.is_finite() || range.mMaximum <= 0.0 {
            return Err(AudioError::CaptureFailed {
                status,
                detail: format!(
                    "could not read a finite Core Audio IO buffer maximum: {}",
                    describe_status(status)
                ),
            });
        }
        let frames = range.mMaximum.ceil();
        if frames > usize::MAX as f64 {
            return Err(AudioError::CaptureFailed {
                status: 0,
                detail: "Core Audio IO buffer maximum is too large to allocate".to_owned(),
            });
        }
        let channels = usize::try_from(format.channels).map_err(|_| AudioError::CaptureFailed {
            status: 0,
            detail: "tap channel count does not fit this platform".to_owned(),
        })?;
        let capacity =
            (frames as usize)
                .checked_mul(channels)
                .ok_or_else(|| AudioError::CaptureFailed {
                    status: 0,
                    detail: "Core Audio IO buffer maximum overflows the callback buffer".to_owned(),
                })?;
        if capacity == 0 {
            return Err(AudioError::CaptureFailed {
                status: 0,
                detail: "Core Audio IO buffer maximum is zero".to_owned(),
            });
        }
        Ok(capacity)
    }

    /// Builds a private aggregate device that reads one tap.
    ///
    /// Private, so it does not appear in Sound preferences for whoever is
    /// sitting at the machine, and auto-starting so it follows the tap rather
    /// than needing separate lifecycle management.
    fn create_aggregate_device(tap_uid: &str) -> Result<AudioObjectID, AudioError> {
        let sub_tap_uid_key = NSString::from_str(&key(kAudioSubTapUIDKey));
        let sub_tap_drift_key = NSString::from_str(&key(kAudioSubTapDriftCompensationKey));
        let tap_uid_value = NSString::from_str(tap_uid);
        let drift_value = NSNumber::new_u32(1);

        let sub_tap: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[&*sub_tap_uid_key, &*sub_tap_drift_key],
            &[&*tap_uid_value as &AnyObject, &*drift_value as &AnyObject],
        );
        let tap_list: Retained<NSArray<AnyObject>> =
            NSArray::from_slice(&[&*sub_tap as &AnyObject]);

        // The aggregate takes its clock from the current default output
        // device. Without one it is created successfully and then never runs,
        // producing no callbacks at all rather than an error.
        let output_uid = default_output_device_uid().ok_or_else(|| AudioError::CaptureFailed {
            status: 0,
            detail: "no default output device to clock the capture from".to_owned(),
        })?;
        let output_uid_value = NSString::from_str(&output_uid);

        // `AudioHardware.h` specifies an array of sub-device *dictionaries*,
        // not an array of UID strings. Passing bare strings appeared to work,
        // but relying on undocumented tolerance is how the earlier
        // "created but never runs" failure happened in the first place.
        let sub_device_uid_key = NSString::from_str(&key(kAudioSubDeviceUIDKey));
        let sub_device_drift_key = NSString::from_str(&key(kAudioSubDeviceDriftCompensationKey));
        let no_drift = NSNumber::new_u32(0);
        let sub_device: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[&*sub_device_uid_key, &*sub_device_drift_key],
            &[&*output_uid_value as &AnyObject, &*no_drift as &AnyObject],
        );
        let sub_devices: Retained<NSArray<AnyObject>> =
            NSArray::from_slice(&[&*sub_device as &AnyObject]);

        // A UID unique to this process and moment: two sessions on one machine
        // must not collide on the same aggregate device.
        let uid = format!(
            "pier.arcen.tech.audio.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );

        let name_key = NSString::from_str(&key(kAudioAggregateDeviceNameKey));
        let uid_key = NSString::from_str(&key(kAudioAggregateDeviceUIDKey));
        let private_key = NSString::from_str(&key(kAudioAggregateDeviceIsPrivateKey));
        let stacked_key = NSString::from_str(&key(kAudioAggregateDeviceIsStackedKey));
        let autostart_key = NSString::from_str(&key(kAudioAggregateDeviceTapAutoStartKey));
        let subdevices_key = NSString::from_str(&key(kAudioAggregateDeviceSubDeviceListKey));
        let taplist_key = NSString::from_str(&key(kAudioAggregateDeviceTapListKey));
        let main_key = NSString::from_str(&key(kAudioAggregateDeviceMainSubDeviceKey));
        let clock_key = NSString::from_str(&key(kAudioAggregateDeviceClockDeviceKey));

        let name_value = NSString::from_str("Arcen Pier session audio");
        let uid_value = NSString::from_str(&uid);
        let yes = NSNumber::new_u32(1);
        let no = NSNumber::new_u32(0);

        let description: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[
                &*name_key,
                &*uid_key,
                &*private_key,
                &*stacked_key,
                &*autostart_key,
                &*subdevices_key,
                &*taplist_key,
                &*main_key,
                &*clock_key,
            ],
            &[
                &*name_value as &AnyObject,
                &*uid_value as &AnyObject,
                &*yes as &AnyObject,
                &*no as &AnyObject,
                &*yes as &AnyObject,
                &*sub_devices as &AnyObject,
                &*tap_list as &AnyObject,
                &*output_uid_value as &AnyObject,
                &*output_uid_value as &AnyObject,
            ],
        );

        let mut device: AudioObjectID = 0;
        // SAFETY: `NSDictionary` is toll-free bridged with `CFDictionary`, so
        // reinterpreting the pointer is the documented bridge rather than a
        // guess. The dictionary and everything it holds outlive the call, and
        // `device` is a live local the call fills in.
        let status = unsafe {
            let bridged = Retained::as_ptr(&description).cast::<CFDictionary>();
            AudioHardwareCreateAggregateDevice(&*bridged, NonNull::from(&mut device))
        };
        if status != 0 {
            return Err(AudioError::CaptureFailed {
                status,
                detail: format!("aggregate device refused: {}", describe_status(status)),
            });
        }
        Ok(device)
    }

    /// Whether any process is currently playing to the default output.
    ///
    /// A process tap on a silent device delivers nothing, because there is
    /// nothing to deliver. Without this, a session that starts while the
    /// desktop happens to be quiet cannot tell that from a tap that is broken,
    /// and disables audio for the rest of the session — so sound only ever
    /// worked if something was already playing at the moment of connection.
    #[must_use]
    pub fn output_is_running() -> bool {
        let Some(device) = default_output_device() else {
            return false;
        };
        let address = AudioObjectPropertyAddress {
            mSelector: u32::from_be_bytes(*b"gone"),
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut running: u32 = 0;
        let mut size = u32::try_from(size_of::<u32>()).unwrap_or(0);
        // SAFETY: `device` is a live device id from the lookup above, and the
        // pointers address live locals sized by `size`.
        let status = unsafe {
            AudioObjectGetPropertyData(
                device,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut running).cast(),
            )
        };
        status == 0 && running != 0
    }

    /// Returns the current default output device id.
    fn default_output_device() -> Option<AudioObjectID> {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut device: AudioObjectID = 0;
        let mut size = u32::try_from(size_of::<AudioObjectID>()).unwrap_or(0);
        // SAFETY: the system object id is a documented constant; the pointers
        // address live locals and `size` states the exact buffer size.
        let status = unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut device).cast(),
            )
        };
        (status == 0 && device != 0).then_some(device)
    }

    /// Returns the UID of the current default output device.
    ///
    /// The aggregate device needs a real device to take its clock from.
    /// Without one it is created successfully and then never runs, which
    /// presents as a capture that produces no callbacks at all rather than as
    /// an error — the least diagnosable of the possible failures.
    fn default_output_device_uid() -> Option<String> {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut device: AudioObjectID = 0;
        let mut size = u32::try_from(size_of::<AudioObjectID>()).unwrap_or(0);
        // SAFETY: the system object id is a documented constant. The pointers
        // address live locals and `size` states the exact buffer size.
        let status = unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut device).cast(),
            )
        };
        if status != 0 || device == 0 {
            return None;
        }

        let uid_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyDeviceUID,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut raw: *const CFString = std::ptr::null();
        let mut uid_size = u32::try_from(size_of::<*const CFString>()).unwrap_or(0);
        // SAFETY: `device` was just read from Core Audio and is non-zero. The
        // pointers address live locals and `uid_size` states the exact buffer
        // size offered.
        let status = unsafe {
            AudioObjectGetPropertyData(
                device,
                NonNull::from(&uid_address),
                0,
                std::ptr::null(),
                NonNull::from(&mut uid_size),
                NonNull::from(&mut raw).cast(),
            )
        };
        if status != 0 || raw.is_null() {
            return None;
        }
        // SAFETY: this property follows the create rule, so ownership is taken
        // here and released when the `CFRetained` drops. `raw` is non-null.
        let uid = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw.cast_mut())) };
        Some(uid.to_string())
    }

    /// Renders a Core Audio dictionary key as a Rust string.
    fn key(raw: &CStr) -> String {
        raw.to_string_lossy().into_owned()
    }

    /// Turns an `OSStatus` into something an operator can act on.
    ///
    /// Core Audio packs four-character codes into the status, so the numeric
    /// value alone is not readable.
    fn describe_status(status: i32) -> String {
        let code = status.to_be_bytes();
        if code.iter().all(u8::is_ascii_graphic) {
            let text: String = code.iter().map(|&byte| char::from(byte)).collect();
            match text.as_str() {
                "!pri" => "not permitted: the process lacks audio-capture consent".to_owned(),
                "!obj" => "no such audio object".to_owned(),
                other => format!("Core Audio status '{other}'"),
            }
        } else {
            format!("Core Audio status {status}")
        }
    }

    #[cfg(test)]
    mod allocation_tests {
        #![allow(clippy::expect_used)]

        use super::*;
        use objc2_core_audio_types::AudioBuffer;
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;
        use std::hint::black_box;

        struct CountingAllocator;

        thread_local! {
            static COUNTING: Cell<bool> = const { Cell::new(false) };
            static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
        }

        // SAFETY: this wrapper delegates every allocation operation to
        // `System` unchanged and only increments thread-local counters before
        // the delegating calls, so it preserves `System`'s allocation contract.
        unsafe impl GlobalAlloc for CountingAllocator {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                record_allocation();
                // SAFETY: the caller upholds `GlobalAlloc::alloc`'s layout contract;
                // this wrapper forwards it unchanged to the system allocator.
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                record_allocation();
                // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s layout
                // contract; this wrapper forwards it unchanged.
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: the pointer and layout came from the allocator API's
                // caller contract; this wrapper forwards them unchanged.
                unsafe { System.dealloc(ptr, layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                record_allocation();
                // SAFETY: the caller upholds `GlobalAlloc::realloc`'s pointer,
                // layout, and size contract; this wrapper forwards them unchanged.
                unsafe { System.realloc(ptr, layout, new_size) }
            }
        }

        #[global_allocator]
        static GLOBAL: CountingAllocator = CountingAllocator;

        fn record_allocation() {
            COUNTING.with(|counting| {
                if counting.get() {
                    ALLOCATIONS.with(|allocations| allocations.set(allocations.get() + 1));
                }
            });
        }

        fn count_allocations(run: impl FnOnce()) -> usize {
            ALLOCATIONS.with(|allocations| allocations.set(0));
            COUNTING.with(|counting| counting.set(true));
            run();
            COUNTING.with(|counting| counting.set(false));
            ALLOCATIONS.with(Cell::get)
        }

        fn callback_once(
            samples: &mut [f32],
            counters: &CaptureCounters,
            pending: &Mutex<Vec<f32>>,
            staging: &Mutex<Vec<f32>>,
        ) {
            let byte_size =
                u32::try_from(std::mem::size_of_val(samples)).expect("test buffer size");
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: 2,
                    mDataByteSize: byte_size,
                    mData: samples.as_mut_ptr().cast(),
                }],
            };
            let list = NonNull::from(&mut list);
            // SAFETY: the local `AudioBufferList` describes the live `samples`
            // slice for the duration of the call and contains one interleaved
            // 32-bit-float stereo buffer, matching `accumulate`'s contract.
            unsafe { accumulate(list, counters, pending, staging) };
        }

        fn drain_for_test(pending: &Mutex<Vec<f32>>, drain_buffer: &mut Vec<f32>) {
            assert!(drain_pending_samples(pending, drain_buffer));
            drain_buffer.clear();
        }

        #[test]
        fn repeated_callback_refill_after_drain_does_not_allocate() {
            let counters = CaptureCounters::default();
            let pending = Mutex::new(Vec::<f32>::with_capacity(MAX_PENDING_SAMPLES));
            let staging = Mutex::new(Vec::<f32>::with_capacity(512));
            let mut drain_buffer = Vec::<f32>::with_capacity(MAX_PENDING_SAMPLES);
            let mut samples = vec![0.25f32; 512];

            callback_once(&mut samples, &counters, &pending, &staging);
            drain_for_test(&pending, &mut drain_buffer);

            let mut per_cycle = Vec::new();
            for _ in 0..64 {
                let allocations = count_allocations(|| {
                    callback_once(&mut samples, &counters, &pending, &staging);
                    drain_for_test(&pending, &mut drain_buffer);
                });
                per_cycle.push(allocations);
            }

            eprintln!("callback allocation counts after warmup: {per_cycle:?}");
            assert!(
                per_cycle.iter().all(|&allocations| allocations == 0),
                "callback-side refill after drain allocated: {per_cycle:?}"
            );
            assert_eq!(
                counters.dropped_buffers.load(Ordering::Relaxed),
                0,
                "the no-allocation path should not need to drop within capacity"
            );
            black_box(drain_buffer);
        }

        #[test]
        fn oversized_callback_buffers_are_clamped_and_counted() {
            let counters = CaptureCounters::default();
            let pending = Mutex::new(Vec::<f32>::with_capacity(MAX_PENDING_SAMPLES));
            let staging = Mutex::new(Vec::<f32>::with_capacity(128));
            let mut drain_buffer = Vec::<f32>::with_capacity(MAX_PENDING_SAMPLES);
            let mut samples = vec![0.25f32; 512];

            callback_once(&mut samples, &counters, &pending, &staging);
            drain_for_test(&pending, &mut drain_buffer);
            counters.dropped_buffers.store(0, Ordering::Relaxed);

            let allocations = count_allocations(|| {
                callback_once(&mut samples, &counters, &pending, &staging);
            });

            eprintln!("oversized callback allocation count after warmup: {allocations}");
            assert_eq!(allocations, 0);
            assert_eq!(
                pending.lock().expect("pending queue").len(),
                128,
                "the callback should only publish the preallocated sample budget"
            );
            assert_eq!(counters.dropped_buffers.load(Ordering::Relaxed), 1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::{AudioError, MuteEvidence, TapFormat};
    use arcen_session::pier_config::LocalPlayback;

    /// A tap that cannot exist off macOS.
    #[derive(Debug)]
    pub struct SystemAudioTap;

    impl SystemAudioTap {
        /// Always refuses: there is no Core Audio here.
        ///
        /// # Errors
        ///
        /// Always returns [`AudioError::Unsupported`].
        pub fn create(_local_playback: LocalPlayback) -> Result<Self, AudioError> {
            Err(AudioError::Unsupported(
                "Core Audio process taps exist only on macOS".to_owned(),
            ))
        }

        /// Never reached.
        ///
        /// # Errors
        ///
        /// Always returns [`AudioError::FormatUnavailable`].
        pub fn format(&self) -> Result<TapFormat, AudioError> {
            Err(AudioError::FormatUnavailable { status: 0 })
        }

        /// Never reached.
        #[must_use]
        pub const fn mute_evidence(&self) -> MuteEvidence {
            MuteEvidence {
                requested: false,
                observed_muted: None,
                honoured: false,
            }
        }

        /// Never reached.
        ///
        /// # Errors
        ///
        /// Never returns an error.
        pub fn release(&mut self) -> Result<(), AudioError> {
            Ok(())
        }
    }

    /// A recorder failure after the mute lease has already been established.
    #[derive(Debug)]
    pub struct AudioRecorderStartupError {
        /// The live tap that still owns the local-playback policy.
        pub tap: SystemAudioTap,
        /// Why sample capture could not be started.
        pub source: AudioError,
    }

    /// A recorder that cannot exist off macOS.
    #[derive(Debug)]
    pub struct AudioCaptureSession;

    impl AudioCaptureSession {
        /// Always refuses: there is no Core Audio here.
        ///
        /// # Errors
        ///
        /// Always returns [`AudioError::Unsupported`].
        pub fn start(_local_playback: LocalPlayback) -> Result<Self, AudioError> {
            Err(AudioError::Unsupported(
                "Core Audio process taps exist only on macOS".to_owned(),
            ))
        }

        /// Always returns the tap: only the recorder is unsupported here.
        ///
        /// # Errors
        ///
        /// Always returns [`AudioRecorderStartupError`].
        pub fn start_with_tap(tap: SystemAudioTap) -> Result<Self, AudioRecorderStartupError> {
            Err(AudioRecorderStartupError {
                tap,
                source: AudioError::Unsupported(
                    "Core Audio recording exists only on macOS".to_owned(),
                ),
            })
        }
    }
}

pub use native::{
    AudioCaptureSession, AudioRecorderStartupError, CaptureCounters, SystemAudioTap,
    output_is_running,
};

/// What a run of [`probe`] observed.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AudioProbeReport {
    /// Whether a tap could be created at all.
    pub tap_created: bool,
    /// The format the tap produces, when one was created.
    pub format: Option<TapFormat>,
    /// What happened to local playback.
    pub mute: Option<MuteEvidence>,
    /// Whether local playback was restored afterwards.
    pub restored: bool,
    /// Why it failed, when it did.
    pub error: Option<String>,
    /// What actually came through the tap, when capture was exercised.
    pub capture: Option<CaptureObservation>,
    /// Whether this host may admit a session under the requested policy.
    pub usable: bool,
}

/// What a capture run actually received.
///
/// A callback that runs and hands over silence looks identical to a working
/// capture if only the callback count is recorded, so the peak amplitude is
/// kept alongside it. That is the difference between capture working and
/// capture being permitted but empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CaptureObservation {
    /// How long capture ran, in milliseconds.
    pub duration_ms: u64,
    /// Times Core Audio called the IO proc.
    pub callbacks: u64,
    /// Audio frames delivered.
    pub frames: u64,
    /// Peak absolute sample value, scaled by one million.
    pub peak_micro: u64,
    /// Whether any non-silent audio arrived.
    pub audio_observed: bool,
    /// Complete audio-v1 packets framed from the captured samples.
    pub packets: u64,
    /// Buffers in the final callback: one is interleaved, more is planar.
    pub buffers_last: u64,
    /// Channels in the first buffer of the final callback.
    pub channels_last: u64,
    /// Callbacks whose buffer layout could not be interleaved.
    pub unsupported_layout: u64,
    /// Callbacks skipped because a lock was contended.
    pub contended_buffers: u64,
    /// Callbacks dropped because framing could not keep up.
    pub dropped_buffers: u64,
}

/// Establishes a tap, reads its real format, then releases it.
///
/// This deliberately reports what the machine did rather than whether the
/// calls returned. A tap that is created but whose format cannot be read is a
/// failure, and a tap that cannot be released is a worse one, because it
/// leaves the machine silent.
/// Whether a capture session has ever started successfully in this process.
static CAPTURE_EVER_SUCCEEDED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Returns whether host audio capture has been proven in this process.
///
/// Proven, not configured and not preflighted: the only way to know a tap will
/// deliver is to have had one deliver. Apple raises the system audio consent
/// prompt inside the capture start and offers no way to ask the question
/// cheaply beforehand.
#[must_use]
pub fn capture_has_succeeded() -> bool {
    CAPTURE_EVER_SUCCEEDED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Creates and drops a tap so this bundle has asked for audio capture.
///
/// macOS lists a subject under Privacy & Security only once it has asked, and
/// a Core Audio process tap is what asks; there is no preflight call.
///
/// The tap is created with playback left audible and dropped immediately. A
/// registration that silenced the machine on every start would be a worse bug
/// than the one it fixes, and muting is a session decision.
///
/// **Creating a tap is not evidence that audio will arrive.** Measured on a
/// machine with Screen Recording denied, the tap is created, reports a valid
/// 48 kHz stereo float format, honours a mute request, and then delivers zero
/// callbacks and zero frames; the same probe on a granted machine delivers
/// dozens of packets in under a second. System audio capture is gated by the
/// same consent as screen capture, which is why Apple's pane is called
/// "Screen & System Audio Recording". Anything that needs to know whether
/// audio actually arrives must read `audio_observed` from a real probe rather
/// than infer it from this returning `true`.
///
/// Returns whether a tap could be created at all, which is useful only for
/// distinguishing "no audio hardware or refused outright" from "registered".
#[must_use]
pub fn register_capture_consent() -> CaptureConsent {
    match SystemAudioTap::create(LocalPlayback::Audible) {
        Ok(tap) => {
            drop(tap);
            CaptureConsent::Registered
        }
        Err(_) => CaptureConsent::Unavailable,
    }
}

/// What asking for audio capture achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureConsent {
    /// A tap was created, so the subject is now listed in Privacy & Security.
    ///
    /// This says nothing about whether audio will arrive.
    Registered,
    /// No tap could be created, so there is nothing to grant.
    Unavailable,
}

impl std::fmt::Display for AudioRecorderStartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for AudioRecorderStartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[must_use]
pub fn probe(local_playback: LocalPlayback) -> AudioProbeReport {
    probe_for(local_playback, std::time::Duration::from_millis(750))
}

/// Establishes a tap, captures for `duration`, then releases everything.
///
/// Capture is actually run rather than merely arranged, because a tap that
/// exists is not a tap that delivers. The report separates "a callback fired"
/// from "audio arrived": a permitted-but-silent capture produces callbacks and
/// zero amplitude, and reporting that as success is how a host ends up
/// advertising audio that nobody can hear.
#[must_use]
pub fn probe_for(local_playback: LocalPlayback, duration: std::time::Duration) -> AudioProbeReport {
    let mut session = match AudioCaptureSession::start(local_playback) {
        Ok(session) => session,
        Err(error) => {
            return AudioProbeReport {
                tap_created: false,
                format: None,
                mute: None,
                restored: true,
                error: Some(error.to_string()),
                capture: None,
                usable: false,
            };
        }
    };

    let format = session.format().ok();
    let mute = session.mute_evidence();

    let started = std::time::Instant::now();
    // Drain periodically rather than once at the end, which is what a session
    // does: framing has to keep up with capture or the bounded queue fills and
    // audio is dropped.
    while started.elapsed() < duration {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = session.drain_packets();
    }
    let _ = session.drain_packets();
    let elapsed = started.elapsed();

    let counters = session.counters();
    let observation = CaptureObservation {
        duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        callbacks: counters
            .callbacks
            .load(std::sync::atomic::Ordering::Relaxed),
        frames: counters.frames.load(std::sync::atomic::Ordering::Relaxed),
        peak_micro: counters
            .peak_micro
            .load(std::sync::atomic::Ordering::Relaxed),
        audio_observed: counters
            .peak_micro
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0,
        packets: counters.packets.load(std::sync::atomic::Ordering::Relaxed),
        buffers_last: counters
            .buffers_last
            .load(std::sync::atomic::Ordering::Relaxed),
        channels_last: counters
            .channels_last
            .load(std::sync::atomic::Ordering::Relaxed),
        unsupported_layout: counters
            .unsupported_layout
            .load(std::sync::atomic::Ordering::Relaxed),
        contended_buffers: counters
            .contended_buffers
            .load(std::sync::atomic::Ordering::Relaxed),
        dropped_buffers: counters
            .dropped_buffers
            .load(std::sync::atomic::Ordering::Relaxed),
    };

    let restored = session.stop().is_ok();

    AudioProbeReport {
        tap_created: true,
        format,
        mute: Some(mute),
        restored,
        error: None,
        capture: Some(observation),
        // Deliberately does not require `audio_observed`: a silent desktop
        // produces no audio, and refusing the host because nothing happened to
        // be playing would be wrong. What must hold is that the pipeline ran,
        // the mute was honoured, and the machine got its sound back.
        // Packets are required too: callbacks that arrive in a layout this
        // adapter cannot interleave would otherwise read as success while
        // producing nothing the wire can carry.
        usable: format.is_some()
            && mute.honoured
            && restored
            && observation.callbacks > 0
            && observation.packets > 0,
    }
}

#[cfg(test)]
mod tests {

    /// Serializes the tests that create a real tap.
    ///
    /// There is one default output device on a machine, and a Core Audio
    /// aggregate built around it is not shareable. Two tests creating taps at
    /// once fail intermittently in whichever one loses, which reads as a flaky
    /// audio stack rather than as two tests competing for one device.
    static AUDIO_DEVICE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive_audio() -> std::sync::MutexGuard<'static, ()> {
        AUDIO_DEVICE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    use super::*;

    #[test]
    fn a_muted_policy_is_what_requires_a_lease() {
        assert!(LocalPlayback::Muted.requires_mute());
        assert!(!LocalPlayback::Audible.requires_mute());
    }

    #[test]
    fn a_failed_probe_never_claims_the_host_is_usable() {
        // The failure path must not report a machine as ready for audio. This
        // is the direction that matters: claiming audio works and delivering
        // silence is worse than refusing.
        let report = AudioProbeReport {
            tap_created: false,
            format: None,
            mute: None,
            restored: true,
            error: Some("denied".to_owned()),
            capture: None,
            usable: false,
        };
        assert!(!report.usable);
    }

    #[test]
    fn errors_say_which_problem_occurred() {
        // An operator seeing "unsupported" installs a newer macOS; one seeing
        // "not permitted" grants consent. Collapsing them wastes their time.
        let unsupported = AudioError::Unsupported("too old".to_owned()).to_string();
        let denied = AudioError::TapFailed {
            status: 560_227_702,
            detail: "not permitted".to_owned(),
        }
        .to_string();
        let stuck = AudioError::ReleaseFailed { status: -1 }.to_string();

        assert!(unsupported.contains("unsupported"));
        assert!(denied.contains("not permitted"));
        // The one failure an operator must act on immediately says so.
        assert!(stuck.contains("local audio may still be muted"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recorder_format_contract_is_not_part_of_a_mute_lease() {
        let mismatched = TapFormat {
            sample_rate_hz: 44_100.0,
            channels: 2,
            bits_per_channel: 32,
            float: true,
        };

        let error =
            native::require_capture_format(mismatched, arcen_media::audio::AudioFrameSpec::V1)
                .expect_err("the recorder still refuses formats it cannot packetize honestly");
        assert!(matches!(error, AudioError::UnsupportedFormat { .. }));
        assert!(error.to_string().contains("44100 Hz"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_tap_reports_a_real_format_and_gives_the_sound_back() {
        let _device = exclusive_audio();
        // Runs against real Core Audio. A machine without the entitlement or
        // consent reports that instead, which is evidence rather than a
        // failure of this test.
        let report = probe(LocalPlayback::Muted);
        if !report.tap_created {
            eprintln!(
                "audio tap unavailable on this machine: {}",
                report.error.unwrap_or_default()
            );
            return;
        }
        let format = report.format.expect("a created tap must report its format");
        assert!(
            format.sample_rate_hz > 0.0,
            "a real tap has a real sample rate, got {format:?}"
        );
        assert!(format.channels > 0, "a real tap has channels: {format:?}");
        assert!(
            report.restored,
            "local audio must be restored, or the machine is left silent"
        );

        let mute = report.mute.expect("a created tap reports its mute state");
        assert!(mute.requested, "this probe asked for silence");
        assert_eq!(
            mute.observed_muted,
            Some(true),
            "Core Audio must report the tapped output as muted, not merely accept the request",
        );
        assert!(mute.honoured);

        // A tap that exists but never delivers is the failure this test is
        // really for: it looked like success for as long as only tap creation
        // was checked.
        let capture = report
            .capture
            .expect("capture is exercised, not just arranged");
        assert!(
            capture.callbacks > 0,
            "the IO proc must actually run; a tap without callbacks delivers nothing: {capture:?}",
        );
        assert!(capture.frames > 0, "frames must arrive: {capture:?}");

        // Roughly the tap's own sample rate. Generous bounds, because this is
        // checking that the clock is real rather than measuring it.
        let seconds = capture.duration_ms as f64 / 1000.0;
        let rate = capture.frames as f64 / seconds;
        assert!(
            rate > format.sample_rate_hz * 0.5 && rate < format.sample_rate_hz * 1.5,
            "delivered rate {rate:.0} should resemble the declared {:.0} Hz",
            format.sample_rate_hz,
        );
        assert!(report.usable);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_audible_override_does_not_silence_the_machine() {
        let _device = exclusive_audio();
        // The override exists so an operator can keep local sound. If it
        // silently muted anyway, the setting would be decorative.
        let report = probe(LocalPlayback::Audible);
        if !report.tap_created {
            eprintln!("audio tap unavailable on this machine");
            return;
        }
        let mute = report.mute.expect("mute evidence");
        assert!(!mute.requested);
        assert_eq!(mute.observed_muted, Some(false));
        assert!(mute.honoured);
    }
}
