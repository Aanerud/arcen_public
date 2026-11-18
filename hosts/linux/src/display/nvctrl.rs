// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Reversible NVIDIA ViewPortIn resize of a single-display session, spoken
//! as NV-CONTROL over a proven X connection.
//!
//! The display is opened the way the joined console's layout is
//! (`session::console::connect`): the authority file without following a
//! link and proved on the opened file, the cookie sent only to an X server
//! its socket's peer credentials admit, and every reply parsed, bounded, by
//! `display::nv_control`. No `nvidia-settings` runs as root against a
//! session's X server.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::logging::target;
use crate::session::console::SessionDisplay;
use crate::session::console_layout::{nv_assign, nv_assign_blocking, nv_snapshot};
use crate::LifecycleEmitter;
use arcen_telemetry::{CorrelationId, FieldValue, LifecycleEventKind, StructuredFields};

const MIN_WIDTH: u32 = 320;
const MIN_HEIGHT: u32 = 240;
const MAX_WIDTH: u32 = 16_384;
const MAX_HEIGHT: u32 = 8_640;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub fn new(width: u32, height: u32) -> Result<Self, DisplayError> {
        if !(MIN_WIDTH..=MAX_WIDTH).contains(&width) || !(MIN_HEIGHT..=MAX_HEIGHT).contains(&height)
        {
            return Err(DisplayError::InvalidResolution(width, height));
        }
        if width % 2 != 0 || height % 2 != 0 {
            return Err(DisplayError::InvalidResolution(width, height));
        }
        Ok(Self { width, height })
    }
}

#[derive(Debug, Error)]
pub enum DisplayError {
    #[error("invalid client resolution {0}x{1}")]
    InvalidResolution(u32, u32),
    #[error("cannot parse active output/base mode from MetaMode")]
    InvalidMetaMode,
    #[error("the display {0} is not a local X display")]
    NotLocal(String),
    #[error("the display {0} has no X authority file to open it with")]
    NoAuthority(String),
    #[error("NV-CONTROL failed: {0}")]
    Command(String),
}

/// The single-display session's X server, addressed over NV-CONTROL.
#[derive(Debug, Clone)]
pub struct NvControl {
    target: Target,
}

#[derive(Debug, Clone)]
enum Target {
    Display(SessionDisplay),
    /// No usable display: every request fails with this reason.
    Unusable(Arc<str>),
    #[cfg(test)]
    Fake(Arc<FakeNvControl>),
}

impl NvControl {
    /// The X display `display` (`:N`), opened with the cookie in
    /// `xauthority`, which must belong to `uid` (the session's user; `None`
    /// lets the file's owner stand in, for a display Arcen did not start for
    /// a user) or root.
    pub fn new(display: String, xauthority: Option<String>, uid: Option<u32>) -> Self {
        let target = match (
            arcen_session::console_session::local_display_number(&display),
            xauthority,
        ) {
            (None, _) => Target::Unusable(DisplayError::NotLocal(display).to_string().into()),
            (Some(_), None) => {
                Target::Unusable(DisplayError::NoAuthority(display).to_string().into())
            }
            (Some(number), Some(xauthority)) => Target::Display(SessionDisplay {
                display: format!(":{number}"),
                number,
                xauthority: PathBuf::from(xauthority),
                uid,
            }),
        };
        Self { target }
    }

    pub async fn snapshot(&self) -> Result<String, DisplayError> {
        match &self.target {
            Target::Display(display) => nv_snapshot(display).await.map_err(DisplayError::Command),
            Target::Unusable(reason) => Err(DisplayError::Command(reason.to_string())),
            #[cfg(test)]
            Target::Fake(fake) => fake.snapshot(),
        }
    }

    pub async fn assign(&self, metamode: &str) -> Result<(), DisplayError> {
        match &self.target {
            Target::Display(display) => nv_assign(display, metamode)
                .await
                .map_err(DisplayError::Command),
            Target::Unusable(reason) => Err(DisplayError::Command(reason.to_string())),
            #[cfg(test)]
            Target::Fake(fake) => fake.assign(metamode),
        }
    }

    fn assign_blocking(&self, metamode: &str) -> Result<(), DisplayError> {
        match &self.target {
            Target::Display(display) => {
                nv_assign_blocking(display, metamode).map_err(DisplayError::Command)
            }
            Target::Unusable(reason) => Err(DisplayError::Command(reason.to_string())),
            #[cfg(test)]
            Target::Fake(fake) => fake.assign(metamode),
        }
    }
}

/// A scripted NV-CONTROL for the guard's tests.
#[cfg(test)]
#[derive(Debug, Default)]
struct FakeNvControl {
    current: Mutex<String>,
    fail_assign: bool,
    assign_delay: std::time::Duration,
    assigned: Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeNvControl {
    fn snapshot(&self) -> Result<String, DisplayError> {
        Ok(self.current.lock().unwrap().clone())
    }

    fn assign(&self, metamode: &str) -> Result<(), DisplayError> {
        std::thread::sleep(self.assign_delay);
        if self.fail_assign {
            return Err(DisplayError::Command("refused".to_owned()));
        }
        metamode.clone_into(&mut self.current.lock().unwrap());
        self.assigned.lock().unwrap().push(metamode.to_owned());
        Ok(())
    }
}

impl Drop for MetaModeGuard {
    fn drop(&mut self) {
        self.dispatch_restore(|state| {
            std::thread::Builder::new()
                .name("arcen-display-restore".to_string())
                .spawn(move || MetaModeGuard::run_restore_job(state))
                .map(|_| ())
        });
    }
}

pub struct MetaModeGuard {
    controller: NvControl,
    prior: String,
    resolution: Resolution,
    changed: bool,
    restore_hold: Option<Box<dyn Send>>,
    emitter: LifecycleEmitter,
    correlation_id: CorrelationId,
}

struct RestoreJob {
    controller: NvControl,
    prior: String,
    restore_hold: Option<Box<dyn Send>>,
    emitter: LifecycleEmitter,
    correlation_id: CorrelationId,
}

/// Emits `DISPLAY_ARMED` (1200) only when the transaction actually mutated
/// the active MetaMode; an already-satisfied resize arms no restore
/// obligation and emits nothing.
fn emit_display_armed(
    emitter: &LifecycleEmitter,
    correlation_id: CorrelationId,
    resolution: Resolution,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("display_backend", FieldValue::String("nvctrl".to_string()));
    let _ = fields.insert(
        "policy",
        FieldValue::String("hold_session_permit".to_string()),
    );
    let _ = fields.insert("changed", FieldValue::Boolean(true));
    let _ = fields.insert("width", FieldValue::Integer(i64::from(resolution.width)));
    let _ = fields.insert("height", FieldValue::Integer(i64::from(resolution.height)));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::DisplayArmed,
        correlation_id,
        fields,
    );
}

/// Emits `DISPLAY_RESTORED` (1201) after a verified explicit restore.
fn emit_display_restored(emitter: &LifecycleEmitter, correlation_id: CorrelationId) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("restore_backend", FieldValue::String("nvctrl".to_string()));
    let _ = fields.insert("changed", FieldValue::Boolean(true));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::DisplayRestored,
        correlation_id,
        fields,
    );
}

/// Emits `DISPLAY_RESTORE_FAILED` (1203) for an explicit restore failure or a
/// Drop-triggered fallback failure. `stage` distinguishes the two; Linux has
/// no separate recovery journal, so `journal_pending` reflects only whether
/// this guard will still retry on `Drop` (`stage == "explicit_restore"`) or
/// has already exhausted its only fallback (`stage == "drop_fallback"`).
fn emit_display_restore_failed(
    emitter: &LifecycleEmitter,
    correlation_id: CorrelationId,
    stage: &'static str,
    journal_pending: bool,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("restore_backend", FieldValue::String("nvctrl".to_string()));
    let _ = fields.insert("stage", FieldValue::String(stage.to_string()));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("restore_verification_failed".to_string()),
    );
    let _ = fields.insert("journal_pending", FieldValue::Boolean(journal_pending));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::DisplayRestoreFailed,
        correlation_id,
        fields,
    );
}

/// Emits `WATCHDOG_RESTORE` (1204) once the `Drop`-triggered fallback path
/// (this crate's substitute for a separate watchdog process) has restored
/// display state.
fn emit_watchdog_restore(emitter: &LifecycleEmitter, correlation_id: CorrelationId) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("restore_backend", FieldValue::String("nvctrl".to_string()));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("drop_fallback_restore".to_string()),
    );
    let _ = fields.insert("journal_pending", FieldValue::Boolean(false));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::WatchdogRestore,
        correlation_id,
        fields,
    );
}

impl MetaModeGuard {
    fn dispatch_restore(
        &mut self,
        spawn_worker: impl FnOnce(Arc<Mutex<Option<RestoreJob>>>) -> std::io::Result<()>,
    ) {
        if !self.changed {
            return;
        }

        let state = Arc::new(Mutex::new(Some(RestoreJob {
            controller: self.controller.clone(),
            prior: self.prior.clone(),
            restore_hold: self.restore_hold.take(),
            emitter: self.emitter.clone(),
            correlation_id: self.correlation_id.clone(),
        })));
        match spawn_worker(state.clone()) {
            Ok(()) => self.changed = false,
            Err(error) => {
                tracing::error!(
                    target: target::DISPLAY,
                    %error,
                    prior = %self.prior,
                    "DROP GUARD restore worker spawn failed; using bounded synchronous fallback"
                );
                Self::run_restore_job(state);
                self.changed = false;
            }
        }
    }

    pub(crate) async fn apply(
        controller: NvControl,
        resolution: Resolution,
        emitter: LifecycleEmitter,
        correlation_id: CorrelationId,
    ) -> Result<Self, DisplayError> {
        Self::apply_inner(controller, resolution, None, emitter, correlation_id).await
    }

    fn run_restore_job(state: Arc<Mutex<Option<RestoreJob>>>) {
        let job = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(job) = job else {
            return;
        };
        let RestoreJob {
            controller,
            prior,
            restore_hold: _restore_hold,
            emitter,
            correlation_id,
        } = job;
        match controller.assign_blocking(&prior) {
            Ok(()) => {
                tracing::info!(
                    target: target::DISPLAY,
                    restored = %prior,
                    "NV-CONTROL MetaMode restored by drop guard"
                );
                emit_watchdog_restore(&emitter, correlation_id);
            }
            Err(error) => {
                tracing::error!(
                    target: target::DISPLAY,
                    %error,
                    prior = %prior,
                    "DROP GUARD FAILED to restore prior NV-CONTROL MetaMode"
                );
                emit_display_restore_failed(&emitter, correlation_id, "drop_fallback", false);
            }
        }
    }

    pub(crate) async fn apply_with_hold<T: Send + 'static>(
        controller: NvControl,
        resolution: Resolution,
        hold: T,
        emitter: LifecycleEmitter,
        correlation_id: CorrelationId,
    ) -> Result<Self, DisplayError> {
        Self::apply_inner(
            controller,
            resolution,
            Some(Box::new(hold)),
            emitter,
            correlation_id,
        )
        .await
    }

    async fn apply_inner(
        controller: NvControl,
        resolution: Resolution,
        restore_hold: Option<Box<dyn Send>>,
        emitter: LifecycleEmitter,
        correlation_id: CorrelationId,
    ) -> Result<Self, DisplayError> {
        let prior = controller.snapshot().await?;
        let requested = build_viewport_metamode(&prior, resolution)?;
        let changed = requested != prior;
        let guard = Self {
            controller,
            prior,
            resolution,
            changed,
            restore_hold,
            emitter,
            correlation_id,
        };
        if changed {
            guard.controller.assign(&requested).await?;
            tracing::info!(
                target: target::DISPLAY,
                width = resolution.width,
                height = resolution.height,
                prior = %guard.prior,
                active = %requested,
                "NV-CONTROL ViewPortIn applied"
            );
            emit_display_armed(&guard.emitter, guard.correlation_id.clone(), resolution);
        }
        Ok(guard)
    }

    /// Re-target the active MetaMode to a new resolution mid-session.
    ///
    /// Builds from the original pre-session `prior` snapshot, so the restore
    /// target (explicit or `Drop`-fallback) never moves: disconnect always
    /// returns the desktop to its pre-session MetaMode no matter how many
    /// resizes happened in between. The restore obligation stays armed —
    /// the canonical rebuilt MetaMode string never byte-matches the raw
    /// query snapshot, and a redundant restore to the same raster is
    /// harmless.
    pub async fn reassign(&mut self, resolution: Resolution) -> Result<(), DisplayError> {
        let requested = build_viewport_metamode(&self.prior, resolution)?;
        self.controller.assign(&requested).await?;
        self.resolution = resolution;
        self.changed = true;
        tracing::info!(
            target: target::DISPLAY,
            width = resolution.width,
            height = resolution.height,
            prior = %self.prior,
            active = %requested,
            "NV-CONTROL ViewPortIn reassigned mid-session"
        );
        emit_display_armed(&self.emitter, self.correlation_id.clone(), resolution);
        Ok(())
    }

    /// Returns the physical raster (X11 virtual screen) dimensions by parsing
    /// `ViewPortOut=` from the pre-session MetaMode snapshot.  Returns `None`
    /// only when the prior MetaMode is an unusual format that contains neither
    /// `ViewPortOut=` nor an `@WxH` token.
    pub fn raster_size(&self) -> Option<(u32, u32)> {
        self.prior
            .split_once("ViewPortOut=")
            .and_then(|(_, v)| parse_size_prefix(v))
            .or_else(|| {
                self.prior
                    .split_once('@')
                    .and_then(|(_, v)| parse_size_prefix(v))
            })
    }

    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    pub async fn restore(&mut self) -> Result<(), DisplayError> {
        if !self.changed {
            return Ok(());
        }
        match self.controller.assign(&self.prior).await {
            Ok(()) => {
                tracing::info!(
                    target: target::DISPLAY,
                    restored = %self.prior,
                    "NV-CONTROL MetaMode restored"
                );
                self.changed = false;
                emit_display_restored(&self.emitter, self.correlation_id.clone());
                Ok(())
            }
            Err(error) => {
                // `self.changed` stays `true`: the `Drop` fallback will
                // still attempt to restore this guard's prior MetaMode.
                emit_display_restore_failed(
                    &self.emitter,
                    self.correlation_id.clone(),
                    "explicit_restore",
                    true,
                );
                Err(error)
            }
        }
    }
}

pub fn requested_resolution(width: u32, height: u32) -> Result<Option<Resolution>, DisplayError> {
    match (width, height) {
        (0, 0) => Ok(None),
        (width, height) => {
            let mut resolution = Resolution::new(width, height)?;
            resolution.width &= !3;
            Ok(Some(resolution))
        }
    }
}

fn build_viewport_metamode(prior: &str, resolution: Resolution) -> Result<String, DisplayError> {
    let (output, mode) = prior.split_once(':').ok_or(DisplayError::InvalidMetaMode)?;
    let base_mode = mode
        .split_whitespace()
        .next()
        .filter(|mode| {
            *mode == "nvidia-auto-select"
                || mode.split_once('x').is_some_and(|(width, height)| {
                    width.parse::<u32>().is_ok() && height.parse::<u32>().is_ok()
                })
        })
        .ok_or(DisplayError::InvalidMetaMode)?;
    let (raster_width, raster_height) =
        metamode_raster_size(prior, base_mode).ok_or(DisplayError::InvalidMetaMode)?;
    Ok(format!(
        "{}: {} {{ ViewPortIn={}x{}, ViewPortOut={}x{}+0+0 }}",
        output.trim(),
        base_mode,
        resolution.width,
        resolution.height,
        raster_width,
        raster_height
    ))
}

fn metamode_raster_size(prior: &str, base_mode: &str) -> Option<(u32, u32)> {
    prior
        .split_once("ViewPortOut=")
        .and_then(|(_, value)| parse_size_prefix(value))
        .or_else(|| {
            prior
                .split_once('@')
                .and_then(|(_, value)| parse_size_prefix(value))
        })
        .or_else(|| parse_size_prefix(base_mode))
}

fn parse_size_prefix(value: &str) -> Option<(u32, u32)> {
    let token = value
        .trim_start()
        .split(|character: char| !character.is_ascii_digit() && character != 'x')
        .next()?;
    let (width, height) = token.split_once('x')?;
    let width = width.parse::<u32>().ok()?;
    let height = height.parse::<u32>().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn test_correlation_id() -> CorrelationId {
        CorrelationId::parse_uuid("01234567-89ab-4def-8123-456789abcdef")
            .expect("canonical correlation id")
    }

    /// What NV-CONTROL answers (the driver's `token=value … ::` prefix is
    /// already removed by `display::nv_control`).
    const QUERY: &str = "DPY-0: 3840x2160 @3840x2160 +0+0 {ViewPortIn=3840x2160, \
                         ViewPortOut=3840x2160+0+0}";
    const AUTO_QUERY: &str = "DPY-1: nvidia-auto-select @2560x1600 +0+0 \
                              {ViewPortIn=2560x1600, ViewPortOut=2560x1600+0+0}";
    const PRIOR: &str = "DPY-0: 3840x2160 @3840x2160 +0+0";

    fn scripted(current: &str) -> FakeNvControl {
        FakeNvControl {
            current: Mutex::new(current.to_owned()),
            ..FakeNvControl::default()
        }
    }

    fn controller(fake: &Arc<FakeNvControl>) -> NvControl {
        NvControl {
            target: Target::Fake(Arc::clone(fake)),
        }
    }

    fn armed_guard(fake: &Arc<FakeNvControl>, emitter: LifecycleEmitter) -> MetaModeGuard {
        MetaModeGuard {
            controller: controller(fake),
            prior: PRIOR.to_owned(),
            resolution: Resolution {
                width: 3840,
                height: 2160,
            },
            changed: true,
            restore_hold: None,
            emitter,
            correlation_id: test_correlation_id(),
        }
    }

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    struct RestoreHold(Arc<AtomicBool>);

    impl Drop for RestoreHold {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn validates_even_client_resolution_bounds() {
        assert_eq!(
            Resolution::new(3600, 2338).unwrap(),
            Resolution {
                width: 3600,
                height: 2338
            }
        );
        for (width, height) in [
            (0, 0),
            (319, 240),
            (1921, 1080),
            (1920, 1081),
            (20_000, 1080),
        ] {
            assert!(Resolution::new(width, height).is_err());
        }
    }

    #[test]
    fn zero_pair_means_no_resize_but_partial_zero_is_invalid() {
        assert_eq!(requested_resolution(0, 0).unwrap(), None);
        assert!(requested_resolution(1920, 0).is_err());
    }

    #[test]
    fn requested_resolution_aligns_width_for_yuv444_capture() {
        assert_eq!(
            requested_resolution(1366, 768).unwrap(),
            Some(Resolution {
                width: 1364,
                height: 768,
            })
        );
        assert_eq!(
            requested_resolution(1398, 760).unwrap(),
            Some(Resolution {
                width: 1396,
                height: 760,
            })
        );
    }

    #[test]
    fn builds_viewportin_from_active_output_and_base_mode() {
        let prior = QUERY;
        assert_eq!(
            build_viewport_metamode(prior, Resolution::new(3600, 2338).unwrap()).unwrap(),
            "DPY-0: 3840x2160 { ViewPortIn=3600x2338, ViewPortOut=3840x2160+0+0 }"
        );
    }

    #[test]
    fn fresh_xorg_auto_select_metamode_can_be_resized() {
        let prior = AUTO_QUERY;
        assert_eq!(
            build_viewport_metamode(prior, Resolution::new(3600, 2338).unwrap()).unwrap(),
            "DPY-1: nvidia-auto-select { ViewPortIn=3600x2338, ViewPortOut=2560x1600+0+0 }"
        );
    }

    #[test]
    fn regression_a_three_head_metamode_string_would_collapse_to_one_head_if_ever_passed_through() {
        // This documents exactly why `run_ws` must skip the legacy
        // single-viewport resize entirely for a committed multi-monitor
        // session (`skip_metamode_resize_for_multi_monitor` in
        // `net::server`), rather than ever calling
        // `MetaModeGuard::apply_with_hold`/`build_viewport_metamode` with a
        // dedicated multi-head Xorg display's live MetaModes snapshot:
        // `build_viewport_metamode` only ever understands and preserves a
        // *single* output clause. Fed the full three-head MetaModes string
        // a real `DedicatedXorg` session's own NV-CONTROL query would
        // return, it silently drops the second and third heads instead of
        // rejecting the extra clauses outright.
        let three_head_prior = "DFP-0: 1920x1080 +0+0 {ViewPortIn=1920x1080, \
                                 ViewPortOut=1920x1080+0+0}, \
                                 DFP-1: 1920x1080 +1920+0 {ViewPortIn=1920x1080, \
                                 ViewPortOut=1920x1080+0+0}, \
                                 DFP-2: 1920x1080 +3840+0 {ViewPortIn=1920x1080, \
                                 ViewPortOut=1920x1080+0+0}";
        let corrupted =
            build_viewport_metamode(three_head_prior, Resolution::new(1920, 1080).unwrap())
                .unwrap();
        assert_eq!(
            corrupted,
            "DFP-0: 1920x1080 { ViewPortIn=1920x1080, ViewPortOut=1920x1080+0+0 }"
        );
        assert!(
            !corrupted.contains("DFP-1") && !corrupted.contains("DFP-2"),
            "a real assign() with this string would silently drop 2 of the 3 applied heads"
        );
    }

    #[test]
    fn raster_size_prefers_explicit_viewportout_then_at_mode_then_base_mode() {
        assert_eq!(
            metamode_raster_size("DPY-1: auto @1920x1080 {ViewPortOut=2560x1600+0+0}", "auto"),
            Some((2560, 1600))
        );
        assert_eq!(
            metamode_raster_size("DPY-1: auto @1920x1080 +0+0", "auto"),
            Some((1920, 1080))
        );
        assert_eq!(
            metamode_raster_size("DPY-1: 3840x2160 +0+0", "3840x2160"),
            Some((3840, 2160))
        );
    }

    #[test]
    fn a_display_without_an_authority_file_or_not_local_is_refused_before_any_request() {
        let none = NvControl::new(":0".to_owned(), None, Some(1000));
        assert!(
            matches!(none.target, Target::Unusable(ref reason) if reason.contains("X authority"))
        );
        let remote = NvControl::new("host:0".to_owned(), Some("/x".to_owned()), None);
        assert!(
            matches!(remote.target, Target::Unusable(ref reason) if reason.contains("not a local"))
        );
        let local = NvControl::new(
            ":7.0".to_owned(),
            Some("/run/a/Xauthority".to_owned()),
            Some(1000),
        );
        let Target::Display(display) = local.target else {
            panic!("a local display with an authority file is usable");
        };
        assert_eq!(
            display,
            SessionDisplay {
                display: ":7".to_owned(),
                number: 7,
                xauthority: PathBuf::from("/run/a/Xauthority"),
                uid: Some(1000),
            }
        );
        let error = none.assign_blocking(PRIOR).unwrap_err().to_string();
        assert!(error.contains("X authority"), "{error}");
    }

    #[test]
    fn armed_drop_guard_restores_prior_metamode() {
        let fake = Arc::new(scripted(QUERY));
        drop(armed_guard(&fake, LifecycleEmitter::disabled()));
        assert!(wait_until(|| !fake.assigned.lock().unwrap().is_empty()));
        assert_eq!(*fake.assigned.lock().unwrap(), vec![PRIOR.to_owned()]);
    }

    #[tokio::test]
    async fn apply_with_hold_emits_display_armed_only_when_the_metamode_changes() {
        let fake = Arc::new(scripted(QUERY));
        let (emitter, recorded) = LifecycleEmitter::recording();
        let resolution = Resolution::new(3600, 2338).unwrap();

        let mut guard = MetaModeGuard::apply(
            controller(&fake),
            resolution,
            emitter,
            test_correlation_id(),
        )
        .await
        .expect("apply succeeds");
        assert!(guard.changed);
        assert_eq!(
            *fake.assigned.lock().unwrap(),
            vec!["DPY-0: 3840x2160 { ViewPortIn=3600x2338, ViewPortOut=3840x2160+0+0 }".to_owned()]
        );

        let recorded_events = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded_events.len(), 1);
        assert_eq!(
            recorded_events[0].kind(),
            arcen_telemetry::LifecycleEventKind::DisplayArmed
        );
        assert_eq!(
            recorded_events[0].fields().as_map().get("changed"),
            Some(&FieldValue::Boolean(true))
        );
        guard.changed = false;

        // Already at the requested viewport: nothing assigned, nothing armed.
        let satisfied = Arc::new(scripted(
            "DPY-0: 3840x2160 { ViewPortIn=3600x2338, ViewPortOut=3840x2160+0+0 }",
        ));
        let (emitter, recorded) = LifecycleEmitter::recording();
        let guard = MetaModeGuard::apply(
            controller(&satisfied),
            resolution,
            emitter,
            test_correlation_id(),
        )
        .await
        .expect("apply succeeds");
        assert!(!guard.changed);
        assert!(satisfied.assigned.lock().unwrap().is_empty());
        assert!(crate::eventlog::test_support::recorded_events(&recorded).is_empty());
    }

    #[tokio::test]
    async fn reassign_preserves_the_original_restore_target() {
        let fake = Arc::new(scripted(QUERY));
        let (emitter, _recorded) = LifecycleEmitter::recording();

        let mut guard = MetaModeGuard::apply(
            controller(&fake),
            Resolution::new(1512, 982).unwrap(),
            emitter,
            test_correlation_id(),
        )
        .await
        .expect("apply succeeds");
        let original_prior = guard.prior.clone();
        assert!(guard.changed);
        assert_eq!(guard.resolution(), Resolution::new(1512, 982).unwrap());

        // A second resize must keep the pre-session restore target.
        guard
            .reassign(Resolution::new(1512, 944).unwrap())
            .await
            .expect("reassign succeeds");
        assert_eq!(guard.prior, original_prior, "restore target must not move");
        assert!(guard.changed, "restore obligation persists after reassign");
        assert_eq!(guard.resolution(), Resolution::new(1512, 944).unwrap());

        // Even resizing back to the pre-session raster keeps the obligation
        // armed: the canonical rebuilt string never matches the raw query
        // snapshot, and a redundant same-raster restore is harmless.
        guard
            .reassign(Resolution::new(3840, 2160).unwrap())
            .await
            .expect("reassign back to prior succeeds");
        assert_eq!(guard.prior, original_prior);
        assert!(guard.changed, "restore obligation stays armed");
        guard.changed = false;
    }

    #[tokio::test]
    async fn explicit_restore_emits_display_restored_on_success() {
        let fake = Arc::new(scripted(QUERY));
        let (recording_emitter, recorded) = LifecycleEmitter::recording();
        let mut guard = armed_guard(&fake, recording_emitter);

        guard.restore().await.expect("restore succeeds");
        assert!(!guard.changed);
        assert_eq!(*fake.assigned.lock().unwrap(), vec![PRIOR.to_owned()]);

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].kind(),
            arcen_telemetry::LifecycleEventKind::DisplayRestored
        );
    }

    #[tokio::test]
    async fn explicit_restore_emits_display_restore_failed_and_leaves_the_guard_armed() {
        let fake = Arc::new(FakeNvControl {
            fail_assign: true,
            ..scripted(QUERY)
        });
        let (recording_emitter, recorded) = LifecycleEmitter::recording();
        let mut guard = armed_guard(&fake, recording_emitter);

        assert!(guard.restore().await.is_err());
        // The guard stays armed so `Drop`'s fallback still retries.
        assert!(guard.changed);

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].kind(),
            arcen_telemetry::LifecycleEventKind::DisplayRestoreFailed
        );
        assert_eq!(
            recorded[0].fields().as_map().get("journal_pending"),
            Some(&FieldValue::Boolean(true))
        );
        guard.changed = false; // avoid a Drop-triggered background retry in this test
    }

    #[test]
    fn drop_fallback_restore_success_emits_watchdog_restore() {
        let fake = Arc::new(scripted(QUERY));
        let (recording_emitter, recorded) = LifecycleEmitter::recording();
        drop(armed_guard(&fake, recording_emitter));
        assert!(wait_until(|| {
            !crate::eventlog::test_support::recorded_events(&recorded).is_empty()
        }));
        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].kind(),
            arcen_telemetry::LifecycleEventKind::WatchdogRestore
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_restore_worker_is_nonblocking_and_releases_the_hold_after_restoring() {
        let fake = Arc::new(FakeNvControl {
            assign_delay: Duration::from_millis(200),
            ..scripted(QUERY)
        });
        let hold_released = Arc::new(AtomicBool::new(false));
        let mut guard = armed_guard(&fake, LifecycleEmitter::disabled());
        guard.restore_hold = Some(Box::new(RestoreHold(hold_released.clone())));
        let started = Instant::now();
        drop(guard);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(!hold_released.load(Ordering::SeqCst));
        assert!(wait_until(|| hold_released.load(Ordering::SeqCst)));
        assert_eq!(*fake.assigned.lock().unwrap(), vec![PRIOR.to_owned()]);
    }

    #[test]
    fn restore_spawn_failure_uses_the_synchronous_fallback() {
        let fake = Arc::new(scripted(QUERY));
        let hold_released = Arc::new(AtomicBool::new(false));
        let mut guard = armed_guard(&fake, LifecycleEmitter::disabled());
        guard.restore_hold = Some(Box::new(RestoreHold(hold_released.clone())));

        guard.dispatch_restore(|_| Err(std::io::Error::other("forced spawn failure")));

        assert!(!guard.changed);
        assert!(hold_released.load(Ordering::SeqCst));
        assert_eq!(*fake.assigned.lock().unwrap(), vec![PRIOR.to_owned()]);
    }

    /// Lab evidence against a real NVIDIA X server, as root like the Pier:
    /// `ARCEN_NVCTRL_LAB_DISPLAY=:0 ARCEN_NVCTRL_LAB_XAUTHORITY=<file>
    /// ARCEN_NVCTRL_LAB_UID=<owner> cargo test -p arcen-pier-linux --lib
    /// nvctrl_lab -- --ignored --nocapture`. Reads the MetaMode, assigns
    /// that same MetaMode on the async and the Drop (blocking) path, and
    /// reads it back unchanged.
    #[tokio::test]
    #[ignore = "needs an NVIDIA X server"]
    async fn nvctrl_lab_snapshot_and_reassign_the_current_metamode() {
        let variable = |name: &str| std::env::var(name).expect(name);
        let controller = NvControl::new(
            variable("ARCEN_NVCTRL_LAB_DISPLAY"),
            Some(variable("ARCEN_NVCTRL_LAB_XAUTHORITY")),
            variable("ARCEN_NVCTRL_LAB_UID").parse().ok(),
        );
        let prior = controller.snapshot().await.expect("snapshot");
        println!("snapshot {prior}");
        controller.assign(&prior).await.expect("assign");
        let blocking = controller.clone();
        let same = prior.clone();
        tokio::task::spawn_blocking(move || blocking.assign_blocking(&same))
            .await
            .unwrap()
            .expect("assign on the Drop path");
        assert_eq!(controller.snapshot().await.expect("read back"), prior);
        println!("read back unchanged");
    }
}
