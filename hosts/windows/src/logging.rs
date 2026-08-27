//! Canonical JSON Lines observability and reloadable process controls.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arcen_observability::{InstalledObservability, ObservabilityBuilder, ObservabilityHandle};
use arcen_telemetry::{
    names, OperationalProfile, TelemetryComponent, TelemetryPlatform, TelemetryRole,
};

#[cfg(windows)]
use crate::service::WindowsLogFile;

#[cfg(not(windows))]
#[derive(Clone)]
pub(crate) struct WindowsLogFile;

#[cfg(not(windows))]
impl WindowsLogFile {
    pub(crate) fn reopen(&self) -> Result<(), String> {
        Ok(())
    }

    pub(crate) fn flush(&self) -> Result<(), String> {
        Ok(())
    }
}

pub const NET: &str = names::target::NET;
pub const AUTH: &str = names::target::AUTH;
pub const SESSION: &str = names::target::SESSION;
pub const INPUT: &str = names::target::HID;
pub const CAPENC: &str = "arcen::capenc";
pub const AUDIO: &str = names::target::MEDIA;
pub const DISPLAY: &str = names::target::DISPLAY;
pub const HEALTH: &str = names::target::HEALTH;
pub const CPPIPE: &str = "arcen::cppipe";
pub const EVENTLOG: &str = "arcen::eventlog";

pub const COMPONENT_BROKER: &str = "pier_broker";
pub const COMPONENT_SESSION_AGENT: &str = names::component::SESSION_AGENT;
pub const COMPONENT_DIAGNOSTIC: &str = "pier_diagnostic";
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct LogController {
    handle: ObservabilityHandle,
    log_file: Option<WindowsLogFile>,
    installed: Arc<InstalledObservability>,
}

impl LogController {
    pub fn reload_profile(&self, profile: OperationalProfile) -> Result<(), String> {
        self.handle
            .reload_profile_with(profile, None::<String>)
            .map_err(|error| format!("reload observability profile: {error}"))
    }

    pub fn reload_configured(&self, profile: OperationalProfile) -> Result<(), String> {
        let arcen_log = match std::env::var("ARCEN_LOG") {
            Ok(value) if !value.trim().is_empty() => Some(value),
            Ok(_) | Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(format!("read ARCEN_LOG tracing filter: {error}")),
        };
        self.handle
            .reload_profile_with(profile, arcen_log)
            .map_err(|error| format!("reload observability profile: {error}"))
    }

    pub fn reopen_log(&self) -> Result<(), String> {
        self.log_file
            .as_ref()
            .ok_or_else(|| "this process has no managed log file".to_string())?
            .reopen()
    }

    /// Bounded-flushes every observability sink, then the managed file handle.
    ///
    /// This is repeatable and leaves workers available for later records.
    pub fn flush_log(&self) -> Result<(), String> {
        flush_runtime_and_file(
            |timeout| {
                self.installed
                    .flush(timeout)
                    .map_err(|error| format!("flush observability sinks: {error}"))
            },
            self.log_file.as_ref(),
            SHUTDOWN_FLUSH_TIMEOUT,
        )
    }

    pub fn handle(&self) -> ObservabilityHandle {
        self.handle.clone()
    }
}

fn flush_runtime_and_file(
    flush_runtime: impl FnOnce(Duration) -> Result<(), String>,
    log_file: Option<&WindowsLogFile>,
    timeout: Duration,
) -> Result<(), String> {
    let runtime_result = flush_runtime(timeout);
    let file_result = log_file.map_or(Ok(()), WindowsLogFile::flush);
    match (runtime_result, file_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(runtime), Ok(())) => Err(runtime),
        (Ok(()), Err(file)) => Err(file),
        (Err(runtime), Err(file)) => Err(format!("{runtime}; {file}")),
    }
}

pub fn init(
    profile: OperationalProfile,
    component: &'static str,
    log_file: Option<WindowsLogFile>,
    native_event_log: bool,
) -> Result<LogController, String> {
    let diagnostic = component == COMPONENT_DIAGNOSTIC;
    let component = TelemetryComponent::new(component)
        .map_err(|error| format!("invalid observability component: {error}"))?;
    let mut builder = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component,
        TelemetryPlatform::Windows,
        profile,
    );
    builder = match log_file.as_ref() {
        #[cfg(windows)]
        Some(file) => builder.canonical_writer("canonical_file", file.writer()),
        #[cfg(not(windows))]
        Some(_) => builder.canonical_writer("canonical_stdout", std::io::stdout()),
        None if diagnostic => builder.canonical_writer("canonical_stderr", std::io::stderr()),
        None => builder.canonical_writer("canonical_stdout", std::io::stdout()),
    };

    #[cfg(windows)]
    if native_event_log {
        if let Ok(sink) = crate::eventlog::WindowsEventLogSink::register(
            crate::eventlog::RealWin32EventLogApi,
            crate::eventlog::EVENT_PROVIDER,
        ) {
            builder = builder
                .canonical_queue_capacity(crate::eventlog::MAX_PENDING_EVENTS)
                .register_sink("windows_event_log", sink);
        }
    }
    #[cfg(not(windows))]
    let _ = native_event_log;

    let runtime = builder
        .build()
        .map_err(|error| format!("build observability runtime: {error}"))?;
    let installed = runtime
        .install_global()
        .map_err(|error| format!("install observability runtime: {error}"))?;
    let installed = Arc::new(installed);
    let handle = installed.handle();
    tracing::dispatcher::get_default(|_| {});
    Ok(LogController {
        handle,
        log_file,
        installed,
    })
}

pub(crate) fn canonical_timestamp() -> Result<String, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock predates Unix epoch".to_string())?;
    Ok(format_timestamp(duration))
}

fn format_timestamp(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let days = i64::try_from(total_seconds / 86_400).unwrap_or(i64::MAX);
    let seconds = total_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:06}Z",
        duration.subsec_micros()
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    #[derive(Clone, Default)]
    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn canonical_timestamp_has_frozen_utc_microsecond_shape() {
        let timestamp = format_timestamp(Duration::new(1_774_540_800, 123_456_789));
        assert_eq!(timestamp, "2026-03-26T16:00:00.123456Z");
        assert_eq!(timestamp.len(), 27);
    }

    #[test]
    fn canonical_targets_and_components_validate() {
        for target in [
            NET, AUTH, SESSION, INPUT, CAPENC, AUDIO, DISPLAY, HEALTH, CPPIPE, EVENTLOG,
        ] {
            assert!(arcen_telemetry::TelemetryTarget::new(target).is_ok());
        }
        for component in [
            COMPONENT_BROKER,
            COMPONENT_SESSION_AGENT,
            COMPONENT_DIAGNOSTIC,
        ] {
            assert!(TelemetryComponent::new(component).is_ok());
        }
    }

    #[test]
    fn orderly_flush_invokes_runtime_with_the_bounded_timeout() {
        let calls = AtomicUsize::new(0);
        flush_runtime_and_file(
            |timeout| {
                assert_eq!(timeout, SHUTDOWN_FLUSH_TIMEOUT);
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            None,
            SHUTDOWN_FLUSH_TIMEOUT,
        )
        .expect("flush");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bounded_flush_is_repeatable_and_runtime_remains_live() {
        let writer = RecordingWriter::default();
        let output = Arc::clone(&writer.0);
        let runtime = ObservabilityBuilder::new(
            TelemetryRole::Host,
            TelemetryComponent::new(COMPONENT_SESSION_AGENT).expect("component"),
            TelemetryPlatform::Windows,
            OperationalProfile::Debug,
        )
        .canonical_writer("windows_flush_test", writer)
        .build()
        .expect("runtime");
        let handle = runtime.handle();

        runtime.with_default(|| {
            tracing::info!(target: SESSION, marker = 1_u64, "before first flush");
        });
        flush_runtime_and_file(
            |timeout| {
                handle
                    .flush(timeout)
                    .map_err(|error| format!("flush observability sinks: {error}"))
            },
            None,
            SHUTDOWN_FLUSH_TIMEOUT,
        )
        .expect("first flush");
        runtime.with_default(|| {
            tracing::info!(target: SESSION, marker = 2_u64, "after first flush");
        });
        flush_runtime_and_file(
            |timeout| {
                handle
                    .flush(timeout)
                    .map_err(|error| format!("flush observability sinks: {error}"))
            },
            None,
            SHUTDOWN_FLUSH_TIMEOUT,
        )
        .expect("second flush");

        let output = String::from_utf8(output.lock().expect("lock").clone()).expect("UTF-8");
        assert!(output.contains("before first flush"));
        assert!(output.contains("after first flush"));
        runtime
            .guard()
            .shutdown(Duration::from_secs(1))
            .expect("shutdown");
    }
}
