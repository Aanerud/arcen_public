//! Deck adapter for the shared observability runtime.

pub mod diagnostics;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arcen_observability::{
    InstalledObservability, LifecycleContext, ObservabilityBuilder, ObservabilityHandle,
};
use arcen_telemetry::{
    names, CorrelationId, FieldValue, HealthState, LifecycleEventKind, OperationalProfile,
    StructuredFields, TelemetryComponent, TelemetryPlatform, TelemetryRole, TelemetryTarget,
    ValidatedLifecycleEvent,
};

pub use arcen_telemetry::OperationalProfile as Profile;

pub mod target {
    pub const TRANSPORT: &str = arcen_telemetry::names::target::NET;
    pub const TLS: &str = arcen_telemetry::names::target::AUTH;
    pub const SESSION: &str = arcen_telemetry::names::target::SESSION;
    pub const VIDEO: &str = arcen_telemetry::names::target::MEDIA;
    pub const AUDIO: &str = arcen_telemetry::names::target::MEDIA;
    pub const USB: &str = arcen_telemetry::names::target::HID;
    pub const UI: &str = "arcen::ui";
    pub const INPUT: &str = "arcen::input";
    pub const HEALTH: &str = arcen_telemetry::names::target::HEALTH;
    pub const AUTH: &str = arcen_telemetry::names::target::AUTH;
}

struct SharedWriter<W> {
    inner: Arc<Mutex<W>>,
}

impl<W> Clone for SharedWriter<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W> SharedWriter<W> {
    fn new(writer: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }
}

impl<W: Write> Write for SharedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("Deck log writer poisoned"))?
            .write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("Deck log writer poisoned"))?
            .flush()
    }
}

struct LogState {
    installed: InstalledObservability,
    dir: PathBuf,
    process_sid: CorrelationId,
    started: Instant,
    file_writer: SharedWriter<tracing_appender::rolling::RollingFileAppender>,
    proof_stop: Arc<AtomicBool>,
    #[cfg(test)]
    proof_emissions: Arc<AtomicU64>,
    proof_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

static STATE: OnceLock<LogState> = OnceLock::new();

#[must_use]
pub const fn default_profile() -> OperationalProfile {
    if cfg!(debug_assertions) {
        OperationalProfile::Info
    } else {
        OperationalProfile::Critical
    }
}

/// The profile to start with, honouring `ARCEN_LOG_LEVEL` when it names one.
///
/// A release Deck logs at `critical`, which is right for ordinary use and
/// wrong the moment anyone needs to see what a session negotiated: the
/// records that answer "what did the Deck ask for, and what did it get" are
/// `info` and `debug`, and there was no way to turn them on without a debug
/// build. The hosts already take a numeric level from their config; this is
/// the client's equivalent.
///
/// Accepts a level number (`0`..`3`) or a profile name. An unreadable value
/// falls back to the default rather than failing to start, because losing
/// logging must never cost someone their session.
#[must_use]
pub fn startup_profile() -> OperationalProfile {
    let Some(raw) = std::env::var_os("ARCEN_LOG_LEVEL") else {
        return default_profile();
    };
    let value = raw.to_string_lossy().trim().to_ascii_lowercase();
    match value.as_str() {
        "0" | "critical" => OperationalProfile::Critical,
        "1" | "error" => OperationalProfile::Error,
        "2" | "info" => OperationalProfile::Info,
        "3" | "debug" => OperationalProfile::Debug,
        _ => default_profile(),
    }
}

pub fn init(profile: OperationalProfile) -> Option<PathBuf> {
    if let Some(state) = STATE.get() {
        return Some(state.dir.clone());
    }
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let file_writer = SharedWriter::new(tracing_appender::rolling::daily(&dir, "arcen-client.log"));
    let mut builder = ObservabilityBuilder::new(
        TelemetryRole::Client,
        TelemetryComponent::new(names::component::DECK).ok()?,
        TelemetryPlatform::Macos,
        profile,
    )
    .canonical_writer("deck-json", file_writer.clone());
    if cfg!(debug_assertions) {
        builder = builder.human_console_writer("deck-stderr", std::io::stderr());
    }
    let runtime = builder.build().ok()?;
    let installed = runtime.install_global().ok()?;
    let process_sid = fresh_correlation_id().ok()?;
    let proof_stop = Arc::new(AtomicBool::new(false));
    let proof_emissions = Arc::new(AtomicU64::new(0));
    let state = LogState {
        installed,
        dir: dir.clone(),
        process_sid: process_sid.clone(),
        started: Instant::now(),
        file_writer,
        proof_stop: Arc::clone(&proof_stop),
        #[cfg(test)]
        proof_emissions: Arc::clone(&proof_emissions),
        proof_worker: Mutex::new(None),
    };
    STATE.set(state).ok()?;
    if let Some(state) = STATE.get() {
        let proof_sid = state.process_sid.clone();
        let worker = std::thread::Builder::new()
            .name("deck-observability-proof".to_owned())
            .spawn(move || {
                while !proof_stop.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_secs(60));
                    if proof_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let mut fields = StructuredFields::default();
                    let _ = fields.insert(
                        "overall_state",
                        FieldValue::String("unavailable".to_owned()),
                    );
                    emit(
                        LifecycleEventKind::HealthSnapshot,
                        proof_sid.clone(),
                        EventIdentity::default(),
                        fields,
                        target::HEALTH,
                        "sixty-second Deck process proof of life",
                        None,
                    );
                    proof_emissions.fetch_add(1, Ordering::Release);
                }
            })
            .ok();
        *state
            .proof_worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = worker;
    }

    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "version",
        FieldValue::String(env!("CARGO_PKG_VERSION").to_owned()),
    );
    let _ = fields.insert("os", FieldValue::String(std::env::consts::OS.to_owned()));
    let _ = fields.insert(
        "arch",
        FieldValue::String(std::env::consts::ARCH.to_owned()),
    );
    emit(
        LifecycleEventKind::ClientStart,
        process_sid.clone(),
        EventIdentity::default(),
        fields,
        target::HEALTH,
        "Deck client started",
        None,
    );
    emit_effective_profile(process_sid, profile, "startup");
    Some(dir)
}

pub fn set_level(profile: OperationalProfile) {
    let Some(state) = STATE.get() else {
        return;
    };
    if state.installed.handle().reload_profile(profile).is_ok() {
        emit_effective_profile(state.process_sid.clone(), profile, "user_setting");
    }
}

pub fn handle() -> Option<ObservabilityHandle> {
    STATE.get().map(|state| state.installed.handle())
}

pub fn shutdown() {
    let Some(state) = STATE.get() else {
        return;
    };
    state.proof_stop.store(true, Ordering::Release);
    if let Some(worker) = state
        .proof_worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        worker.thread().unpark();
        let _ = worker.join();
    }
    let before = state
        .installed
        .handle()
        .sink_stats()
        .into_iter()
        .find(|stats| stats.name == "deck-json")
        .map_or(0, |stats| stats.delivered);
    let mut fields = StructuredFields::default();
    let uptime_ms = i64::try_from(state.started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let _ = fields.insert("uptime_ms", FieldValue::Integer(uptime_ms));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("clean_shutdown".to_owned()),
    );
    emit(
        LifecycleEventKind::ClientStop,
        state.process_sid.clone(),
        EventIdentity::default(),
        fields,
        target::HEALTH,
        "Deck client stopped",
        None,
    );

    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        let delivered = state
            .installed
            .handle()
            .sink_stats()
            .into_iter()
            .find(|stats| stats.name == "deck-json")
            .map_or(before, |stats| stats.delivered);
        if delivered > before {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if let Ok(mut writer) = state.file_writer.inner.try_lock() {
        let _ = writer.flush();
    }
}

#[derive(Debug, Clone, Default)]
pub struct EventIdentity {
    pub user: Option<String>,
    pub host: Option<String>,
    pub peer_addr: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn emit(
    kind: LifecycleEventKind,
    sid: CorrelationId,
    identity: EventIdentity,
    fields: StructuredFields,
    target_name: &str,
    message: &str,
    health_state: Option<HealthState>,
) {
    let Some(handle) = handle() else {
        return;
    };
    emit_with_handle(
        &handle,
        kind,
        sid,
        identity,
        fields,
        target_name,
        message,
        health_state,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_with_handle(
    handle: &ObservabilityHandle,
    kind: LifecycleEventKind,
    sid: CorrelationId,
    identity: EventIdentity,
    fields: StructuredFields,
    target_name: &str,
    message: &str,
    health_state: Option<HealthState>,
) {
    let Ok(event) = ValidatedLifecycleEvent::new(kind, sid.clone(), fields) else {
        return;
    };
    let Ok(target) = TelemetryTarget::new(target_name) else {
        return;
    };
    let _ = handle.emit_lifecycle(
        &event,
        LifecycleContext {
            sid,
            user: identity.user,
            host: identity.host,
            peer_addr: identity.peer_addr,
            health_state,
        },
        canonical_now(),
        target,
        message,
    );
}

fn emit_effective_profile(sid: CorrelationId, profile: OperationalProfile, source: &'static str) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "profile_level",
        FieldValue::Integer(i64::from(u8::from(profile))),
    );
    let _ = fields.insert(
        "profile_name",
        FieldValue::String(profile.as_str().to_owned()),
    );
    let _ = fields.insert("profile_source", FieldValue::String(source.to_owned()));
    emit(
        LifecycleEventKind::EffectiveProfile,
        sid,
        EventIdentity::default(),
        fields,
        target::HEALTH,
        "effective observability profile selected",
        None,
    );
}

pub fn current_log_dir() -> Option<PathBuf> {
    STATE.get().map(|state| state.dir.clone()).or_else(log_dir)
}

pub fn log_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ARCEN_LOG_DIR") {
        return Some(PathBuf::from(dir));
    }

    let home = PathBuf::from(std::env::var_os("HOME")?);
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library").join("Logs").join("Arcen"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(home.join(".local").join("state").join("arcen").join("logs"))
    }
}

#[cfg(test)]
pub(crate) fn trigger_proof_for_test() -> Option<u64> {
    let state = STATE.get()?;
    let before = state.proof_emissions.load(Ordering::Acquire);
    let guard = state
        .proof_worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.as_ref()?.thread().unpark();
    drop(guard);
    let deadline = Instant::now() + Duration::from_secs(1);
    while state.proof_emissions.load(Ordering::Acquire) == before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    Some(state.proof_emissions.load(Ordering::Acquire))
}

fn fresh_correlation_id() -> Result<CorrelationId, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)?;
    Ok(CorrelationId::from_uuid_v4_bytes(bytes))
}

fn canonical_now() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs();
    let micros = duration.subsec_micros();
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60
    )
}

const fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_profile_mapping_are_stable() {
        assert_eq!(
            OperationalProfile::try_from(0),
            Ok(OperationalProfile::Critical)
        );
        assert_eq!(
            OperationalProfile::try_from(1),
            Ok(OperationalProfile::Error)
        );
        assert_eq!(
            OperationalProfile::try_from(2),
            Ok(OperationalProfile::Info)
        );
        assert_eq!(
            OperationalProfile::try_from(3),
            Ok(OperationalProfile::Debug)
        );
        assert_eq!(
            default_profile(),
            if cfg!(debug_assertions) {
                OperationalProfile::Info
            } else {
                OperationalProfile::Critical
            }
        );
    }

    #[test]
    fn canonical_timestamp_has_schema_shape() {
        let timestamp = canonical_now();
        assert_eq!(timestamp.len(), 27);
        assert!(timestamp.ends_with('Z'));
    }

    #[test]
    fn shared_profile_reloads_without_reinstalling_runtime() {
        let runtime = ObservabilityBuilder::new(
            TelemetryRole::Client,
            TelemetryComponent::new(names::component::DECK).unwrap(),
            TelemetryPlatform::Macos,
            OperationalProfile::Critical,
        )
        .canonical_writer("profile", SharedWriter::new(Vec::<u8>::new()))
        .build()
        .unwrap();
        let handle = runtime.handle();
        assert_eq!(handle.profile().unwrap(), OperationalProfile::Critical);
        handle
            .reload_profile(OperationalProfile::Debug)
            .expect("live profile reload");
        assert_eq!(handle.profile().unwrap(), OperationalProfile::Debug);
    }

    #[test]
    fn real_deck_writer_emits_exact_level_zero_json_and_flushes() {
        let writer = SharedWriter::new(Vec::<u8>::new());
        let runtime = ObservabilityBuilder::new(
            TelemetryRole::Client,
            TelemetryComponent::new(names::component::DECK).unwrap(),
            TelemetryPlatform::Macos,
            OperationalProfile::Critical,
        )
        .arcen_log(Some("arcen=off"))
        .canonical_writer("fixture", writer.clone())
        .build()
        .unwrap();
        let handle = runtime.handle();
        let sid = CorrelationId::from_uuid_v4_bytes([7; 16]);
        let mut fields = StructuredFields::default();
        fields
            .insert("version", FieldValue::String("0.1.0".to_owned()))
            .unwrap();
        fields
            .insert("os", FieldValue::String("macos".to_owned()))
            .unwrap();
        fields
            .insert("arch", FieldValue::String("aarch64".to_owned()))
            .unwrap();
        let event =
            ValidatedLifecycleEvent::new(LifecycleEventKind::ClientStart, sid.clone(), fields)
                .unwrap();
        handle
            .emit_lifecycle(
                &event,
                LifecycleContext {
                    sid,
                    user: None,
                    host: None,
                    peer_addr: None,
                    health_state: None,
                },
                "2026-07-24T16:00:00.000000Z",
                TelemetryTarget::new(target::HEALTH).unwrap(),
                "Deck client started",
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while handle.sink_stats()[0].delivered == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(runtime);
        let bytes = writer.inner.lock().unwrap().clone();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            concat!(
                "{\"schema_version\":1,\"timestamp\":\"2026-07-24T16:00:00.000000Z\",",
                "\"sequence\":1,\"profile_level\":0,\"profile_name\":\"critical\",",
                "\"severity\":\"info\",\"role\":\"client\",\"component\":\"deck\",",
                "\"platform\":\"macos\",\"target\":\"arcen::health\",\"event_id\":1500,",
                "\"event_name\":\"CLIENT_START\",\"category\":\"health\",\"outcome\":\"succeeded\",",
                "\"sid\":\"07070707-0707-4707-8707-070707070707\",",
                "\"user\":null,\"host\":null,\"peer_addr\":null,\"health_state\":null,",
                "\"message\":\"Deck client started\",",
                "\"fields\":{\"arch\":\"aarch64\",\"os\":\"macos\",\"version\":\"0.1.0\"}}\n"
            )
        );
    }

    #[test]
    fn lifecycle_order_sid_and_explicit_identity_are_preserved() {
        let writer = SharedWriter::new(Vec::<u8>::new());
        let runtime = ObservabilityBuilder::new(
            TelemetryRole::Client,
            TelemetryComponent::new(names::component::DECK).unwrap(),
            TelemetryPlatform::Macos,
            OperationalProfile::Info,
        )
        .canonical_writer("lifecycle", writer.clone())
        .build()
        .unwrap();
        let handle = runtime.handle();
        let sid = CorrelationId::from_uuid_v4_bytes([9; 16]);
        let identity = EventIdentity {
            user: Some("artist".to_owned()),
            host: Some("pier.example".to_owned()),
            peer_addr: Some("192.0.2.4:18444".to_owned()),
        };
        let mut attempt = StructuredFields::default();
        attempt
            .insert("transport", FieldValue::String("wss".to_owned()))
            .unwrap();
        emit_with_handle(
            &handle,
            LifecycleEventKind::ClientConnectAttempt,
            sid.clone(),
            identity.clone(),
            attempt,
            target::TRANSPORT,
            "attempt",
            None,
        );
        let mut connected = StructuredFields::default();
        connected
            .insert("tls_version", FieldValue::String("tls1.3".to_owned()))
            .unwrap();
        emit_with_handle(
            &handle,
            LifecycleEventKind::ClientConnectOk,
            sid,
            identity,
            connected,
            target::TRANSPORT,
            "connected",
            None,
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while handle.sink_stats()[0].delivered < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(runtime);
        let bytes = writer.inner.lock().unwrap().clone();
        let records: Vec<serde_json::Value> = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event_id"], 1502);
        assert_eq!(records[1]["event_id"], 1503);
        assert_eq!(records[0]["sequence"], 1);
        assert_eq!(records[1]["sequence"], 2);
        assert_eq!(records[0]["sid"], records[1]["sid"]);
        assert_eq!(records[1]["user"], "artist");
        assert_eq!(records[1]["host"], "pier.example");
        assert_eq!(records[1]["peer_addr"], "192.0.2.4:18444");
    }
}
