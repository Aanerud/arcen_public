// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keeping the host's own speakers silent for a session
//! (`audio.local_playback`).
//!
//! The policy, the evidence and what to undo are shared
//! (`arcen_session::local_playback`, the endpoint-mute lease). Windows has no
//! session sink to route playback to, so the lease mutes every active render
//! endpoint (`IAudioEndpointVolume::SetMute`) while WASAPI loopback, which the
//! session's audio is captured from, still carries the mix (measured on a lab
//! host: a 440 Hz tone captured at the same level muted and unmuted).
//!
//! The lease is the broker's, not the session agent's: it starts before the
//! agent and ends only after the session is locked
//! (`console_session::DETACH_STEPS_OUTPUTS_FIRST`), so the desk stays silent
//! for the detached time too. A device that appears, or that someone unmutes,
//! is muted again within a second. Each mute is journalled
//! (`recovery\local-playback.json`) before it is applied; a journal a stopped
//! or killed broker left behind is undone at the next service start. Only
//! what the lease muted is unmuted: a device the user had muted stays muted.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use arcen_session::local_playback::{
    endpoint_report, endpoints_to_mute, endpoints_to_unmute, EndpointMuteJournal,
    LocalPlaybackReport, Sink,
};
use arcen_session::pier_config::LocalPlayback;

use crate::logging::AUDIO;

/// How often the lease mutes a device that appeared or was unmuted.
const ENFORCE_INTERVAL: Duration = Duration::from_secs(1);
/// How long the first pass may take before the session is refused.
const FIRST_PASS_TIMEOUT: Duration = Duration::from_secs(10);
const JOURNAL_MAX_BYTES: u64 = 64 * 1024;

fn journal_path() -> std::path::PathBuf {
    crate::paths::recovery_dir().join("local-playback.json")
}

fn read_journal() -> Option<EndpointMuteJournal> {
    let text = std::fs::read_to_string(journal_path()).ok()?;
    match serde_json::from_str::<EndpointMuteJournal>(&text) {
        Ok(journal) if journal.is_supported() => Some(journal),
        Ok(_) | Err(_) => {
            tracing::warn!(
                target: AUDIO,
                path = %journal_path().display(),
                "local playback journal is unreadable; it is left in place"
            );
            None
        }
    }
}

fn write_journal(journal: &EndpointMuteJournal) -> Result<(), String> {
    let payload = serde_json::to_vec(journal)
        .map_err(|error| format!("serialize local playback journal: {error}"))?;
    crate::recovery::write_atomic_bytes(
        &journal_path(),
        &payload,
        JOURNAL_MAX_BYTES,
        "local playback journal",
    )
}

fn remove_journal() {
    match std::fs::remove_file(journal_path()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            target: AUDIO,
            %error,
            "could not remove the local playback journal"
        ),
    }
}

/// The audio system's render endpoints, as the shared lease reads them.
trait Endpoints {
    fn read(&mut self) -> Result<Vec<Sink>, String>;
    fn set_mute(&mut self, endpoint: &str, muted: bool) -> Result<(), String>;
}

/// Mutes every endpoint that plays, journalling each first. Returns what the
/// devices report afterwards.
fn enforce(
    endpoints: &mut impl Endpoints,
    journal: &mut EndpointMuteJournal,
    persist: &mut impl FnMut(&EndpointMuteJournal) -> Result<(), String>,
) -> Result<Vec<Sink>, String> {
    let before = endpoints.read()?;
    for name in endpoints_to_mute(&before) {
        // On disk before every mute (another lease ending may have removed
        // the file), and in memory only once it is on disk.
        let mut next = journal.clone();
        next.muted.insert(name.clone());
        persist(&next)?;
        *journal = next;
        endpoints.set_mute(&name, true)?;
    }
    endpoints.read()
}

/// Unmutes what `journal` records the lease muted. `Ok` when nothing is
/// left to undo.
fn undo(endpoints: &mut impl Endpoints, journal: &EndpointMuteJournal) -> Result<usize, String> {
    let now = endpoints.read()?;
    let targets = endpoints_to_unmute(journal, &now);
    let mut failures = Vec::new();
    for name in &targets {
        if let Err(error) = endpoints.set_mute(name, false) {
            failures.push(error);
        }
    }
    if failures.is_empty() {
        Ok(targets.len())
    } else {
        Err(failures.join("; "))
    }
}

/// A running endpoint-mute lease.
pub(crate) struct LocalPlaybackLease {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl LocalPlaybackLease {
    /// Starts the lease `policy` asks for and reports what it proved. A lease
    /// is returned only when something must be undone at its end.
    pub(crate) fn begin(policy: LocalPlayback) -> (Option<Self>, LocalPlaybackReport) {
        if !policy.requires_mute() {
            return (None, LocalPlaybackReport::not_requested());
        }
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let (first, first_report) = mpsc::channel();
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("local-playback".to_string())
            .spawn(move || {
                platform::run_lease(&worker_stop, &first);
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                tracing::error!(target: AUDIO, %error, "could not start the local playback lease");
                return (None, endpoint_report(None, true));
            }
        };
        let lease = Self {
            stop,
            thread: Some(thread),
        };
        let report = first_report
            .recv_timeout(FIRST_PASS_TIMEOUT)
            .unwrap_or_else(|_| endpoint_report(None, true));
        (Some(lease), report)
    }

    /// Ends the lease: what it muted is unmuted, the journal removed.
    pub(crate) fn end(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        let (flag, wake) = &*self.stop;
        if let Ok(mut stop) = flag.lock() {
            *stop = true;
        }
        wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LocalPlaybackLease {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Undoes a journal a stopped or killed broker left. Runs at service start,
/// before any session.
pub(crate) fn recover_left_over() {
    let Some(journal) = read_journal() else {
        return;
    };
    match platform::undo_journal(&journal) {
        Ok(unmuted) => {
            tracing::info!(
                target: AUDIO,
                unmuted,
                "left-over local playback lease undone at service start"
            );
            remove_journal();
        }
        Err(error) => tracing::warn!(
            target: AUDIO,
            %error,
            "left-over local playback lease could not be undone; journal kept"
        ),
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::mpsc;
    use std::sync::{Condvar, Mutex};

    use arcen_session::local_playback::{
        endpoint_report, EndpointMuteJournal, LocalPlaybackReport, Sink,
    };
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{
        eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    };

    use super::{
        enforce, read_journal, remove_journal, undo, write_journal, Endpoints, ENFORCE_INTERVAL,
    };
    use crate::logging::AUDIO;

    struct Com;

    impl Com {
        fn initialize() -> Result<Self, String> {
            // SAFETY: balanced by `CoUninitialize` in `Drop` on this thread.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .map_err(|error| format!("CoInitializeEx: {error}"))?;
            Ok(Self)
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: this thread initialised COM in `initialize`.
            unsafe { CoUninitialize() };
        }
    }

    struct RenderEndpoints {
        enumerator: IMMDeviceEnumerator,
    }

    impl RenderEndpoints {
        fn new() -> Result<Self, String> {
            // SAFETY: COM is initialised on this thread.
            let enumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("MMDeviceEnumerator: {error}"))?;
            Ok(Self { enumerator })
        }

        fn devices(&self) -> Result<Vec<(String, IMMDevice)>, String> {
            // SAFETY: the enumerator is a live COM object on this thread.
            let collection = unsafe {
                self.enumerator
                    .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            }
            .map_err(|error| format!("EnumAudioEndpoints: {error}"))?;
            // SAFETY: as above.
            let count = unsafe { collection.GetCount() }
                .map_err(|error| format!("IMMDeviceCollection::GetCount: {error}"))?;
            let mut devices = Vec::with_capacity(count as usize);
            for index in 0..count {
                // SAFETY: `index` is below the collection's count.
                let device = unsafe { collection.Item(index) }
                    .map_err(|error| format!("IMMDeviceCollection::Item: {error}"))?;
                devices.push((endpoint_id(&device)?, device));
            }
            Ok(devices)
        }

        fn volume(&self, id: &str) -> Result<IAudioEndpointVolume, String> {
            let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            let device = unsafe { self.enumerator.GetDevice(PCWSTR(wide.as_ptr())) }
                .map_err(|error| format!("GetDevice: {error}"))?;
            activate_volume(&device)
        }
    }

    fn endpoint_id(device: &IMMDevice) -> Result<String, String> {
        // SAFETY: the device is live; the returned string is freed below.
        let id = unsafe { device.GetId() }.map_err(|error| format!("IMMDevice::GetId: {error}"))?;
        // SAFETY: GetId returned a NUL-terminated string owned by the caller.
        let text = unsafe { id.to_string() }.map_err(|error| format!("endpoint id: {error}"));
        // SAFETY: the string was allocated by COM for this caller.
        unsafe { CoTaskMemFree(Some(id.0.cast())) };
        text
    }

    fn activate_volume(device: &IMMDevice) -> Result<IAudioEndpointVolume, String> {
        // SAFETY: the device is live on this COM thread.
        unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }
            .map_err(|error| format!("IAudioEndpointVolume: {error}"))
    }

    impl Endpoints for RenderEndpoints {
        fn read(&mut self) -> Result<Vec<Sink>, String> {
            self.devices()?
                .into_iter()
                .map(|(name, device)| {
                    let volume = activate_volume(&device)?;
                    // SAFETY: the interface is live on this thread.
                    let muted = unsafe { volume.GetMute() }
                        .map_err(|error| format!("GetMute: {error}"))?
                        .as_bool();
                    Ok(Sink { name, muted })
                })
                .collect()
        }

        fn set_mute(&mut self, endpoint: &str, muted: bool) -> Result<(), String> {
            let volume = self.volume(endpoint)?;
            // SAFETY: the interface is live; no event context is passed.
            unsafe { volume.SetMute(muted, std::ptr::null()) }
                .map_err(|error| format!("SetMute({muted}): {error}"))
        }
    }

    fn log_report(report: &LocalPlaybackReport, devices: usize) {
        tracing::info!(
            target: AUDIO,
            basis = report.basis.token(),
            requested = report.evidence.requested,
            observed_muted = ?report.evidence.observed_muted,
            honoured = report.evidence.honoured,
            devices,
            "local playback lease"
        );
    }

    pub(super) fn run_lease(
        stop: &(Mutex<bool>, Condvar),
        first: &mpsc::Sender<LocalPlaybackReport>,
    ) {
        let started = Com::initialize().and_then(|com| Ok((com, RenderEndpoints::new()?)));
        let (_com, mut endpoints) = match started {
            Ok(started) => started,
            Err(error) => {
                tracing::error!(target: AUDIO, %error, "local playback lease could not start");
                let _ = first.send(endpoint_report(None, true));
                return;
            }
        };
        let mut journal = read_journal().unwrap_or_default();
        match enforce(&mut endpoints, &mut journal, &mut write_journal) {
            Ok(devices) => {
                let report = endpoint_report(Some(&devices), true);
                log_report(&report, devices.len());
                let _ = first.send(report);
            }
            Err(error) => {
                tracing::error!(target: AUDIO, %error, "local playback could not be muted");
                let _ = first.send(endpoint_report(None, true));
            }
        }
        let (flag, wake) = stop;
        let mut stopped = flag.lock().map_or(true, |stop| *stop);
        while !stopped {
            if let Ok(guard) = flag.lock() {
                stopped = wake
                    .wait_timeout(guard, ENFORCE_INTERVAL)
                    .map_or(true, |(guard, _)| *guard);
            } else {
                stopped = true;
            }
            if !stopped {
                if let Err(error) = enforce(&mut endpoints, &mut journal, &mut write_journal) {
                    tracing::warn!(
                        target: AUDIO,
                        %error,
                        "local playback re-mute pass failed; retried"
                    );
                }
            }
        }
        match undo(&mut endpoints, &journal) {
            Ok(unmuted) => {
                tracing::info!(target: AUDIO, unmuted, "local playback given back");
                remove_journal();
            }
            Err(error) => tracing::warn!(
                target: AUDIO,
                %error,
                "local playback could not be given back; the journal is kept for the next start"
            ),
        }
    }

    pub(super) fn undo_journal(journal: &EndpointMuteJournal) -> Result<usize, String> {
        let _com = Com::initialize()?;
        let mut endpoints = RenderEndpoints::new()?;
        undo(&mut endpoints, journal)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::sync::mpsc;
    use std::sync::{Condvar, Mutex};

    use arcen_session::local_playback::{
        endpoint_report, EndpointMuteJournal, LocalPlaybackReport,
    };

    pub(super) fn run_lease(
        _stop: &(Mutex<bool>, Condvar),
        first: &mpsc::Sender<LocalPlaybackReport>,
    ) {
        let _ = first.send(endpoint_report(None, true));
    }

    pub(super) fn undo_journal(_journal: &EndpointMuteJournal) -> Result<usize, String> {
        Err("audio endpoints are only available on Windows".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeEndpoints {
        devices: Vec<Sink>,
        refuse_mute: bool,
    }

    impl Endpoints for FakeEndpoints {
        fn read(&mut self) -> Result<Vec<Sink>, String> {
            Ok(self.devices.clone())
        }

        fn set_mute(&mut self, endpoint: &str, muted: bool) -> Result<(), String> {
            if self.refuse_mute {
                return Err("refused".to_string());
            }
            let device = self
                .devices
                .iter_mut()
                .find(|device| device.name == endpoint)
                .ok_or_else(|| "gone".to_string())?;
            device.muted = muted;
            Ok(())
        }
    }

    fn sink(name: &str, muted: bool) -> Sink {
        Sink {
            name: name.to_string(),
            muted,
        }
    }

    #[test]
    fn a_lease_mutes_every_device_and_gives_back_only_what_it_muted() {
        let mut endpoints = FakeEndpoints {
            devices: vec![sink("speakers", false), sink("headphones", true)],
            ..FakeEndpoints::default()
        };
        let mut journal = EndpointMuteJournal::default();
        let mut persisted = Vec::new();
        let mut persist = |journal: &EndpointMuteJournal| {
            persisted.push(journal.muted.clone());
            Ok(())
        };
        let after = enforce(&mut endpoints, &mut journal, &mut persist).expect("enforce");
        assert!(endpoint_report(Some(&after), true).evidence.honoured);
        // A device plugged in during the session is muted on the next pass.
        endpoints.devices.push(sink("hdmi", false));
        enforce(&mut endpoints, &mut journal, &mut persist).expect("enforce again");
        assert_eq!(persisted.len(), 2, "each mute is journalled first");
        assert_eq!(persisted[1], journal.muted);
        assert!(endpoints.devices.iter().all(|device| device.muted));
        assert_eq!(undo(&mut endpoints, &journal), Ok(2));
        assert!(!endpoints.devices[0].muted);
        assert!(endpoints.devices[1].muted, "the user's own mute stays");
        assert!(!endpoints.devices[2].muted);
    }

    #[test]
    fn a_device_is_never_muted_before_it_is_on_disk() {
        let mut endpoints = FakeEndpoints {
            devices: vec![sink("speakers", false)],
            ..FakeEndpoints::default()
        };
        let mut journal = EndpointMuteJournal::default();
        let mut fail = |_: &EndpointMuteJournal| Err("disk full".to_string());
        assert!(enforce(&mut endpoints, &mut journal, &mut fail).is_err());
        assert!(!endpoints.devices[0].muted, "not muted without a journal");
        assert!(journal.muted.is_empty(), "retried on the next pass");
        let mut written = Vec::new();
        let mut persist = |journal: &EndpointMuteJournal| {
            written.push(journal.muted.clone());
            Ok(())
        };
        enforce(&mut endpoints, &mut journal, &mut persist).expect("next pass");
        assert_eq!(written.len(), 1);
        assert!(endpoints.devices[0].muted);
    }

    #[test]
    fn a_device_that_will_not_mute_is_journalled_and_is_no_evidence() {
        let mut endpoints = FakeEndpoints {
            devices: vec![sink("speakers", false)],
            refuse_mute: true,
        };
        let mut journal = EndpointMuteJournal::default();
        assert!(enforce(
            &mut endpoints,
            &mut journal,
            &mut |_: &EndpointMuteJournal| Ok(())
        )
        .is_err());
        assert!(journal.muted.contains("speakers"));
        let report = endpoint_report(Some(&endpoints.devices), true);
        assert!(
            arcen_session::local_playback::admit(LocalPlayback::Muted, Some(report.evidence))
                .is_err()
        );
    }
}
