// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The service's work: recover at start, then serve one verified Deck at a
//! time — claim a prepared tablet, bridge it through the shared exporter and
//! session, and put it back when the Deck goes away.
//!
//! Each session holds the one lease of the shared
//! `arcen_usb_bridge::component_lease` policy, granted for the console
//! session and user verified at accept. Session notifications
//! ([`session_changed`]), a console poll, the client's exit and a stop
//! revoke it; the session then tells the Deck why, stops serving, cancels
//! its transfers and gives the tablet its own driver back. A screen lock by
//! the same user keeps the lease.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arcen_usb_bridge::component_ipc::{TAG_ERROR, encode_error, write_frame};
use arcen_usb_bridge::component_lease::{Lease, LeaseAuthority, Revocation, SessionChange};
use arcen_usb_bridge::component_session::{
    SessionEnd, SessionRevocation, SessionStop, UrbCodec, serve_session,
};
use arcen_usb_bridge::winusb_package::TabletModel;
use arcen_usb_bridge::{CapturedUsbDevice, UsbExporter};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;

use super::claim::{self, Journal, Package};
use super::pipe::{self, ClientPolicy, Connection};
use super::winusb::WinUsbDevice;
use crate::codec::ProtocolUrbCodec;

/// How long a connected Deck may stay silent before its first request.
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a session's watcher looks at the console and the client.
const WATCH_STEP: Duration = Duration::from_millis(50);
/// How long a revoked session may take to deliver its notice before the
/// stream is aborted anyway (a Deck that stopped reading).
const NOTICE_GRACE: Duration = Duration::from_millis(300);

/// The component's one lease.
static AUTHORITY: Mutex<LeaseAuthority> = Mutex::new(LeaseAuthority::new());
/// The served session's lease and revocation, for the control handler.
static CURRENT: Mutex<Option<(u64, Arc<SessionRevocation>)>> = Mutex::new(None);
/// The running service's log, for revocations from the control handler.
static LEASE_LOG: std::sync::OnceLock<Log> = std::sync::OnceLock::new();

fn authority() -> std::sync::MutexGuard<'static, LeaseAuthority> {
    AUTHORITY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn current() -> std::sync::MutexGuard<'static, Option<(u64, Arc<SessionRevocation>)>> {
    CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Ends the served session if it holds `revoked`'s lease.
fn revoke_current(revoked: Option<(Lease, Revocation)>) {
    let Some((lease, revocation)) = revoked else {
        return;
    };
    let session = current()
        .as_ref()
        .filter(|(id, _)| *id == lease.id)
        .map(|(_, session)| Arc::clone(session));
    let reason = revocation.to_string();
    if let Some(log) = LEASE_LOG.get() {
        log.line(&format!(
            "lease {} (console session {}) revoked: {reason}",
            lease.id, lease.console_session
        ));
    }
    if let Some(session) = session {
        session.revoke(&reason);
    }
}

/// A `SERVICE_CONTROL_SESSIONCHANGE` notification (`WTS_*` code, session).
pub(crate) fn session_changed(event: u32, session: u32) {
    let change = SessionChange::from_wts(event, session);
    let revoked = authority().on_change(change);
    revoke_current(revoked);
}
/// The log is cut in half when it grows past this.
const LOG_LIMIT: u64 = 1024 * 1024;

/// Where the service keeps its state, and who may connect.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) root: PathBuf,
    pub(crate) policy: ClientPolicy,
    /// Lab only: serve an emulated tablet instead of claiming one.
    pub(crate) emulate: bool,
    /// Also print log lines (console mode).
    pub(crate) echo: bool,
}

impl Config {
    /// The installed configuration: the component root, and the Deck
    /// executables listed in `allowed-clients.txt` (one path per line;
    /// written by the installer) or the default install path.
    pub(crate) fn installed(root: PathBuf) -> Self {
        let list = root.join("allowed-clients.txt");
        // Only an administrator's list is honoured; otherwise the default.
        let trusted = list.exists() && super::common::require_trusted(&list).is_ok();
        let mut allowed: Vec<PathBuf> = trusted
            .then(|| std::fs::read_to_string(&list).ok())
            .flatten()
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        if allowed.is_empty() {
            let program_files = std::env::var_os("ProgramFiles")
                .map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from);
            allowed.push(
                program_files
                    .join("Arcen")
                    .join("Deck")
                    .join("arcen-deck.exe"),
            );
        }
        Self {
            root,
            policy: ClientPolicy {
                allowed_images: allowed,
                #[cfg(feature = "lab-emulate")]
                any_session: false,
            },
            emulate: false,
            echo: false,
        }
    }

    pub(crate) fn journal(&self) -> Journal {
        Journal::new(self.root.join("journal.json"))
    }

    pub(crate) fn packages(&self) -> PathBuf {
        self.root.join("packages")
    }
}

/// A bounded, append-only log beside the journal.
#[derive(Clone)]
pub(crate) struct Log {
    path: PathBuf,
    echo: bool,
    lock: Arc<Mutex<()>>,
}

impl Log {
    pub(crate) fn new(root: &Path, echo: bool) -> Self {
        Self {
            path: root.join("service.log"),
            echo,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn line(&self, text: &str) {
        let _guard = self.lock.lock();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let line = format!("{stamp} {text}\r\n");
        if self.echo {
            eprint!("{line}");
        }
        if std::fs::metadata(&self.path).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
            if let Ok(old) = std::fs::read(&self.path) {
                let _ = std::fs::write(&self.path, &old[old.len() / 2..]);
            }
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// Recovers, then serves Decks until `stop` is raised.
pub(crate) fn run(config: &Config, stop: &AtomicBool) {
    let log = Log::new(&config.root, config.echo);
    let _ = LEASE_LOG.set(log.clone());
    log.line(&format!(
        "arcen-usb-service {} starting; allowed Deck: {:?}",
        env!("CARGO_PKG_VERSION"),
        config.policy.allowed_images
    ));
    for line in claim::recover(&config.journal()) {
        log.line(&line);
    }
    while !stop.load(Ordering::SeqCst) {
        match pipe::accept(&config.policy, stop, || authority().generation()) {
            Ok(connection) => {
                if stop.load(Ordering::SeqCst) {
                    connection.handle().abort();
                    pipe::forget_active();
                    break;
                }
                log.line(&format!(
                    "Deck connected: pid {} {}",
                    connection.client_pid,
                    connection.client_image.display()
                ));
                serve_one(config, &log, &connection);
                pipe::forget_active();
                log.line("Deck disconnected; tablet released");
            }
            Err(refusal) => {
                pipe::forget_active();
                log.line(&refusal);
                if refusal.contains("another process may own the name") {
                    // Never fight for the pipe name: a squatter means the
                    // service must not serve at all.
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }
    log.line("arcen-usb-service stopping");
}

/// Ends a blocked `accept` or a running session so the loop can see
/// `stop`; a session is revoked and ends through its normal path, which
/// restores the tablet's driver.
pub(crate) fn wake() {
    let revoked = authority().revoke_any(Revocation::ComponentStopping);
    if revoked.is_some() {
        // A served session ends through its revocation (the Deck reads the
        // notice; the watcher disconnects a Deck that does not).
        revoke_current(revoked);
    } else {
        pipe::abort_active();
    }
}

fn refuse(connection: &Connection, log: &Log, reason: &str) {
    log.line(&format!("refused: {reason}"));
    let _ = write_frame(&mut connection.writer(), TAG_ERROR, &encode_error(reason));
    // Not a disconnect: the Deck reads the reason before the handle closes.
    connection.handle().cancel();
}

fn serve_one(config: &Config, log: &Log, connection: &Connection) {
    // The lease comes first: nothing touches the tablet for a Deck whose
    // console moved while it was verified.
    let revocation = Arc::new(SessionRevocation::new());
    // Granted and published under one AUTHORITY guard (lock order
    // AUTHORITY → CURRENT; nothing takes them the other way round), so a
    // notification cannot revoke the lease before the session can hear it.
    let granted = {
        let mut authority = authority();
        let granted = authority.grant(connection.generation, connection.console_session);
        if let Ok(lease) = &granted {
            *current() = Some((lease.id, Arc::clone(&revocation)));
        }
        granted
    };
    let lease = match granted {
        Ok(lease) => lease,
        Err(refusal) => return refuse(connection, log, &refusal),
    };
    log.line(&format!(
        "lease {} granted for console session {}",
        lease.id, lease.console_session
    ));
    serve_leased(config, log, connection, lease, &revocation);
    authority().release(lease.id);
    current().take();
}

fn serve_leased(
    config: &Config,
    log: &Log,
    connection: &Connection,
    lease: Lease,
    revocation: &Arc<SessionRevocation>,
) {
    let device: Box<dyn CapturedUsbDevice> = if config.emulate {
        match emulated() {
            Ok(device) => device,
            Err(error) => return refuse(connection, log, &error),
        }
    } else {
        match claim_attached(config, log) {
            Ok(device) => device,
            Err(error) => return refuse(connection, log, &error),
        }
    };
    let exporter = match UsbExporter::new(device) {
        Ok(exporter) => Arc::new(exporter),
        Err((refusal, outcome)) => {
            return refuse(
                connection,
                log,
                &format!(
                    "the attached device is not a bridgeable tablet: {refusal} (release: {outcome:?})"
                ),
            );
        }
    };
    let codec: Arc<dyn UrbCodec> = Arc::new(ProtocolUrbCodec);
    let handle = connection.handle();
    let stop = SessionStop {
        // After a revocation notice, only cancel our own I/O so the Deck can
        // still read the notice; the handle closes when the session ends.
        abort: {
            let revocation = Arc::clone(revocation);
            Arc::new(move || {
                if revocation.is_revoked() {
                    handle.cancel();
                } else {
                    handle.abort();
                }
            })
        },
        revocation: Arc::clone(revocation),
    };
    let cleared = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let backend = if config.emulate { "emulated" } else { "winusb" };
    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| watch(connection, lease, &stop, &cleared, &finished));
        // Set however the session ends (a panic included), or the scope
        // would wait for the watcher forever with the tablet claimed.
        let _finish = Finish(&finished);
        serve_session(
            &exporter,
            &codec,
            backend,
            &mut connection.reader(),
            connection.writer(),
            || cleared.store(true, Ordering::Release),
            &stop,
        )
    });
    match outcome {
        Ok(SessionEnd::Closed) => {}
        Ok(SessionEnd::Refused(reason)) => log.line(&format!("session refused: {reason}")),
        Ok(SessionEnd::Revoked(reason)) => log.line(&format!("session revoked: {reason}")),
        Err(error) => log.line(&format!("session failed to start: {error}")),
    }
    // The last reference: the exporter releases the device, whose release
    // hook restores the tablet's own driver.
    drop(exporter);
}

/// Raises the session's `finished` flag when dropped.
struct Finish<'a>(&'a AtomicBool);

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Watches one session until it finishes: the first-request deadline, the
/// console (a poll that backs up session notifications and is the only
/// signal in console mode), the verified client process, and a revoked
/// session that has not ended within [`NOTICE_GRACE`] (its I/O is cancelled,
/// then, after three times that, the client is disconnected).
fn watch(
    connection: &Connection,
    lease: Lease,
    stop: &SessionStop,
    cleared: &AtomicBool,
    finished: &AtomicBool,
) {
    let started = std::time::Instant::now();
    let mut revoked_at = None;
    let mut aborted = false;
    let mut disconnected = false;
    while !finished.load(Ordering::Acquire) {
        if !cleared.load(Ordering::Acquire)
            && started.elapsed() >= FIRST_REQUEST_TIMEOUT
            && !aborted
        {
            (stop.abort)();
            aborted = true;
        }
        // SAFETY: no arguments.
        let console = unsafe { WTSGetActiveConsoleSessionId() };
        // Each guard is dropped before the session is revoked.
        let moved = authority().on_console_observed(console);
        revoke_current(moved);
        if connection.client_exited() {
            let exited = authority().revoke(lease.id, Revocation::ClientExited);
            revoke_current(exited);
        }
        // Fail closed: a lease the authority no longer holds ends the
        // session even if its revocation was never delivered.
        let held = authority().lease().is_some_and(|held| held.id == lease.id);
        if !held && !stop.revocation.is_revoked() {
            stop.revocation
                .revoke("the tablet was released: this Deck's lease is no longer valid");
        }
        if stop.revocation.is_revoked() {
            let since = *revoked_at.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= NOTICE_GRACE && !aborted {
                (stop.abort)();
                aborted = true;
            }
            if since.elapsed() >= NOTICE_GRACE * 3 && !disconnected {
                connection.handle().abort();
                disconnected = true;
            }
        }
        std::thread::sleep(WATCH_STEP);
    }
}

/// Claims the first attached tablet that has a prepared package.
fn claim_attached(config: &Config, log: &Log) -> Result<Box<dyn CapturedUsbDevice>, String> {
    if !config.journal().load().claims.is_empty() {
        for line in claim::recover(&config.journal()) {
            log.line(&line);
        }
    }
    let models = Package::prepared_models(&config.packages());
    if models.is_empty() {
        return Err("no tablet model is prepared for Native tablet on this PC".to_owned());
    }
    let instances = super::devices::present_usb_instances()?;
    let (instance, model) = instances
        .iter()
        .filter(|id| id.matches('\\').count() == 2)
        .find_map(|id| {
            let model = TabletModel::from_device_id(id)?;
            models.contains(&model).then(|| (id.clone(), model))
        })
        .ok_or("no prepared tablet is attached")?;
    let package = Package::load(&config.packages(), model)?;
    log.line(&format!("claiming {instance}"));
    let journal = config.journal();
    let path = claim::claim(&journal, &package, &instance)?;
    let hook_log = log.clone();
    let hook_instance = instance.clone();
    let hook: super::winusb::ReleaseHook = Box::new(move |reason| {
        let outcome = claim::release(&journal, &hook_instance);
        hook_log.line(&format!(
            "released {hook_instance} ({reason}): {}",
            outcome
                .as_ref()
                .map_or_else(String::as_str, |()| "driver restored")
        ));
        outcome
    });
    WinUsbDevice::open(&path, (model.vendor_id, model.product_id), hook)
        .map(|device| Box::new(device) as Box<dyn CapturedUsbDevice>)
        .inspect_err(|_| {
            // Opening failed after the swap: put the tablet back now.
            let _ = claim::release(&config.journal(), &instance);
        })
}

/// Lab only: `ARCEN_USB_EMULATE_DEVICE=generic-pen` serves a standard HID
/// pen no host has a dedicated reader for (Q-16) instead of the Intuos5.
#[cfg(feature = "lab-emulate")]
fn emulated() -> Result<Box<dyn CapturedUsbDevice>, String> {
    if std::env::var("ARCEN_USB_EMULATE_DEVICE").as_deref() == Ok("generic-pen") {
        return emulated_generic_pen();
    }
    let (device, handle) =
        arcen_usb_bridge::SyntheticDevice::emulate(arcen_usb_bridge::WacomIntuos5::default())
            .map_err(|error| format!("emulated tablet: {error:?}"))?;
    // A lab stroke every few seconds, delivered only when the host polls.
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(3));
            for step in arcen_usb_bridge::intuos5_scripted_stroke() {
                if !handle.deliver_when_polled(
                    arcen_usb_bridge::INTUOS5_PEN_IN,
                    step.report.encode().to_vec(),
                    Duration::from_secs(30),
                ) {
                    return;
                }
                std::thread::sleep(arcen_usb_bridge::SCRIPTED_MINIMUM_INTERVAL);
            }
        }
    });
    Ok(Box::new(device))
}

#[cfg(feature = "lab-emulate")]
fn emulated_generic_pen() -> Result<Box<dyn CapturedUsbDevice>, String> {
    let (device, handle) =
        arcen_usb_bridge::SyntheticDevice::emulate(arcen_usb_bridge::GenericPenTablet::default())
            .map_err(|error| format!("emulated generic pen: {error:?}"))?;
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(3));
            for step in arcen_usb_bridge::generic_pen_scripted_stroke() {
                if !handle.deliver_when_polled(
                    arcen_usb_bridge::GENERIC_PEN_IN,
                    step.state.encode().to_vec(),
                    Duration::from_secs(30),
                ) {
                    return;
                }
                std::thread::sleep(arcen_usb_bridge::SCRIPTED_MINIMUM_INTERVAL);
            }
        }
    });
    Ok(Box::new(device))
}

#[cfg(not(feature = "lab-emulate"))]
fn emulated() -> Result<Box<dyn CapturedUsbDevice>, String> {
    Err("this build has no emulated tablet".to_owned())
}
