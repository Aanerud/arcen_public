//! Real, host-free native fullscreen multi-window diagnostic.
//!
//! `arcen-deck multi-monitor-window-diagnostic [1|2|4]` is the visual
//! counterpart to `arcen-deck multi-monitor-harness`: instead of only
//! exercising `crate::pipeline::monitor_router::MonitorFrameRouter` in
//! memory, it opens real, additional native macOS windows (via
//! `crate::ui::multi_window_runtime`'s plan/transaction engine +
//! `egui::Context::show_viewport_immediate` + `ViewportBuilder::with_monitor`
//! / `with_fullscreen`) and drives each one's genuine `NSWindow` fullscreen
//! transition with a distinct, deterministic solid-fill pattern from
//! `crate::pipeline::synthetic_multi_monitor`, routed through a real
//! `MonitorFrameRouter` exactly like the harness does.
//!
//! This proves, without a speculative rewrite and without any real host,
//! that the existing eframe/egui/winit/AppKit stack can genuinely open one
//! native fullscreen window per negotiated monitor (task 6). It is entirely
//! independent of `crate::ui::multi_window::multi_window_runtime_available`
//! (which now returns `true` in the live GUI session path, gating
//! production `ServerHello` handling, per-window input routing, and
//! topology-change teardown -- see `crate::ui::multi_window_activation`):
//! this diagnostic never reads or flips that flag, since it drives the
//! window/plan/router machinery directly for physical, host-free
//! verification rather than through a live session.
//!
//! Only ever reachable from this explicit CLI diagnostic subcommand; the
//! normal GUI/session path (`crate::ui::run_native_app`) never calls this.

use std::time::Duration;

use arcen_media::SessionMonitorId;

/// One viewport's self-reported claim about which monitor it is presenting,
/// gathered from *inside* that viewport's own paint callback -- the root's
/// `eframe::App::ui`, or a `show_viewport_immediate` closure body -- never
/// assumed or forwarded from data planned/computed outside that callback.
///
/// `painted_tag` is the exact synthetic pixel tag byte
/// (`crate::pipeline::synthetic_multi_monitor::monitor_pixel_tag`) read back
/// from the very `DecodedVideoFrame` the callback painted from this frame,
/// so a callback that claims the right monitor id but actually consumed a
/// *different* monitor's routed frame is still caught by
/// [`verify_paint_isolation`], not just a callback that claims the wrong id
/// outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportPaintAck {
    pub viewport_id: egui::ViewportId,
    pub claimed_monitor_id: SessionMonitorId,
    pub painted_tag: u8,
}

/// A failure proving isolation was *not* verified, found strictly from
/// [`ViewportPaintAck`]s the paint callbacks themselves reported -- never
/// from colors/ids planned or pushed from outside a callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintIsolationError {
    /// No callback reported an acknowledgement for this expected viewport
    /// this frame (it never painted, or the diagnostic never asked it to).
    MissingAck(egui::ViewportId),
    /// A callback reported an acknowledgement for a viewport that was not
    /// part of the expected plan at all.
    UnexpectedAck(egui::ViewportId),
    /// This viewport's own callback claimed a *different* monitor id than
    /// the one the plan actually assigned it -- the core "swapped callback
    /// routing" failure this check exists to catch.
    WrongMonitorClaim {
        viewport_id: egui::ViewportId,
        expected_monitor_id: SessionMonitorId,
        claimed_monitor_id: SessionMonitorId,
    },
    /// The callback claimed the right monitor id, but the pixel tag it
    /// actually painted does not match that monitor's own deterministic tag
    /// -- it consumed the wrong routed frame despite the correct claim.
    PaintedTagDoesNotMatchClaim {
        viewport_id: egui::ViewportId,
        claimed_monitor_id: SessionMonitorId,
        painted_tag: u8,
    },
    /// Two different viewports painted the exact same tag -- direct proof
    /// of cross-routing regardless of what either one claimed.
    CrossRoutedTag {
        first: egui::ViewportId,
        second: egui::ViewportId,
        tag: u8,
    },
}

impl std::fmt::Display for PaintIsolationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingAck(viewport_id) => write!(
                formatter,
                "viewport {viewport_id:?} never acknowledged a paint this frame"
            ),
            Self::UnexpectedAck(viewport_id) => write!(
                formatter,
                "viewport {viewport_id:?} acknowledged a paint outside the expected plan"
            ),
            Self::WrongMonitorClaim {
                viewport_id,
                expected_monitor_id,
                claimed_monitor_id,
            } => write!(
                formatter,
                "viewport {viewport_id:?} claimed monitor {} but was assigned monitor {}",
                claimed_monitor_id.get(),
                expected_monitor_id.get()
            ),
            Self::PaintedTagDoesNotMatchClaim {
                viewport_id,
                claimed_monitor_id,
                painted_tag,
            } => write!(
                formatter,
                "viewport {viewport_id:?} claimed monitor {} but painted tag {painted_tag:#04x} \
                 does not match that monitor's own tag",
                claimed_monitor_id.get()
            ),
            Self::CrossRoutedTag { first, second, tag } => write!(
                formatter,
                "viewports {first:?} and {second:?} both painted the identical tag {tag:#04x}"
            ),
        }
    }
}

impl std::error::Error for PaintIsolationError {}

/// Verifies isolation strictly from each expected viewport's own
/// paint-callback acknowledgement (see [`ViewportPaintAck`]) -- never from
/// data planned or pushed from outside a callback. `expected` is the plan's
/// own `(viewport_id, session_monitor_id)` mapping (root included); `acks`
/// is whatever every paint callback actually reported this frame.
///
/// # Errors
///
/// Returns the first [`PaintIsolationError`] found, in this order: a
/// missing ack for an expected viewport, that viewport's claim not matching
/// the plan's own mapping (the "swapped callback routing" case), a painted
/// tag inconsistent with the claim, an ack for a viewport outside the plan,
/// then any pairwise duplicate tag across every remaining ack.
pub fn verify_paint_isolation(
    expected: &[(egui::ViewportId, SessionMonitorId)],
    acks: &[ViewportPaintAck],
) -> Result<(), PaintIsolationError> {
    for &(viewport_id, expected_monitor_id) in expected {
        let Some(ack) = acks.iter().find(|ack| ack.viewport_id == viewport_id) else {
            return Err(PaintIsolationError::MissingAck(viewport_id));
        };
        if ack.claimed_monitor_id != expected_monitor_id {
            return Err(PaintIsolationError::WrongMonitorClaim {
                viewport_id,
                expected_monitor_id,
                claimed_monitor_id: ack.claimed_monitor_id,
            });
        }
        let expected_tag =
            crate::pipeline::synthetic_multi_monitor::monitor_pixel_tag(ack.claimed_monitor_id);
        if ack.painted_tag != expected_tag {
            return Err(PaintIsolationError::PaintedTagDoesNotMatchClaim {
                viewport_id,
                claimed_monitor_id: ack.claimed_monitor_id,
                painted_tag: ack.painted_tag,
            });
        }
    }
    for ack in acks {
        if !expected.iter().any(|&(id, _)| id == ack.viewport_id) {
            return Err(PaintIsolationError::UnexpectedAck(ack.viewport_id));
        }
    }
    for i in 0..acks.len() {
        for j in (i + 1)..acks.len() {
            if acks[i].painted_tag == acks[j].painted_tag {
                return Err(PaintIsolationError::CrossRoutedTag {
                    first: acks[i].viewport_id,
                    second: acks[j].viewport_id,
                    tag: acks[i].painted_tag,
                });
            }
        }
    }
    Ok(())
}

/// Outcome of one `multi-monitor-window-diagnostic` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowDiagnosticReport {
    pub monitor_count: usize,
    /// Session monitor ids (wire ids `1..=monitor_count`) whose window
    /// confirmed a genuine native fullscreen bind before the deadline.
    pub confirmed_session_monitor_ids: Vec<u16>,
    /// Session monitor ids whose window never confirmed before the deadline.
    pub unconfirmed_session_monitor_ids: Vec<u16>,
    /// Every expected viewport (root and every additional native window)
    /// self-reported a paint acknowledgement from *inside* its own paint
    /// callback (see [`ViewportPaintAck`]) that claimed exactly its own
    /// assigned monitor id and painted exactly that monitor's own
    /// deterministic tag, with no two viewports' painted tags equal --
    /// [`verify_paint_isolation`]`.is_ok()`. This is never computed from
    /// colors/ids planned or pushed from outside a callback, so a swapped
    /// callback (one viewport's closure consuming another's routed frame)
    /// is provably caught rather than trusted.
    pub isolation_verified: bool,
}

impl std::fmt::Display for WindowDiagnosticReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "monitors={} confirmed={:?} unconfirmed={:?} isolation_verified={}",
            self.monitor_count,
            self.confirmed_session_monitor_ids,
            self.unconfirmed_session_monitor_ids,
            self.isolation_verified
        )
    }
}

/// Failure running the native window diagnostic.
#[derive(Debug)]
pub enum WindowDiagnosticError {
    InvalidMonitorCount(usize),
    /// Fewer real displays are attached than the requested monitor count.
    /// Never invents synthetic display ids to make up the difference.
    NotEnoughDisplays {
        requested: usize,
        available: usize,
    },
    Plan(crate::ui::multi_window_runtime::MultiWindowPlanError),
    Router(crate::pipeline::monitor_router::RouterBuildError),
    /// A planned window's target display could not be resolved once the
    /// diagnostic was already running (e.g. a display disconnected
    /// mid-run). Triggers a transactional abort: every window is closed and
    /// this is returned instead of a normal report.
    MonitorResolution(crate::ui::multi_window_runtime::MonitorResolutionError),
    /// A definite hard failure was recorded during the transactional enter
    /// -- `close_requested` before commit, an exact observed-display-id
    /// mismatch while a window is genuinely bind-ready, or a display
    /// disappearing out from under its assigned viewport (root or any
    /// additional window). Never waited out to the timeout: every window is
    /// closed immediately and this is returned instead of a normal report,
    /// and this same run never reopens a window afterward.
    Aborted(crate::ui::multi_window_runtime::MultiWindowAbortReason),
    Native(eframe::Error),
    /// This diagnostic only opens real native windows on macOS.
    UnsupportedPlatform,
}

impl std::fmt::Display for WindowDiagnosticError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMonitorCount(count) => {
                write!(formatter, "invalid diagnostic monitor count: {count}")
            }
            Self::NotEnoughDisplays {
                requested,
                available,
            } => write!(
                formatter,
                "requested {requested} monitors but only {available} display(s) are attached"
            ),
            Self::Plan(error) => write!(formatter, "invalid diagnostic window plan: {error}"),
            Self::Router(error) => write!(formatter, "invalid diagnostic router roster: {error}"),
            Self::MonitorResolution(error) => {
                write!(formatter, "diagnostic window aborted: {error}")
            }
            Self::Aborted(reason) => write!(formatter, "diagnostic window aborted: {reason}"),
            Self::Native(error) => write!(formatter, "native window diagnostic failed: {error}"),
            Self::UnsupportedPlatform => formatter
                .write_str("the native multi-window diagnostic only opens real windows on macOS"),
        }
    }
}

impl std::error::Error for WindowDiagnosticError {}

/// Pure decision: whether `monitor_count` exceeds the number of real
/// attached displays. Factored out of `macos::run`'s leading validation so
/// the "not enough displays" rejection is unit-testable without depending
/// on this machine's actual display count, and so it applies before any
/// native window is ever opened.
fn reject_if_not_enough_displays(
    monitor_count: usize,
    available: usize,
) -> Result<(), WindowDiagnosticError> {
    if monitor_count > available {
        return Err(WindowDiagnosticError::NotEnoughDisplays {
            requested: monitor_count,
            available,
        });
    }
    Ok(())
}

/// Runs the native window diagnostic for `monitor_count` (1, 2, or 4)
/// *real* attached displays (in the live active-display list's own
/// deterministic order -- never invented/synthetic ids), waiting up to
/// `timeout` for every window to confirm a genuine native fullscreen bind on
/// its assigned display before closing everything and returning a report.
///
/// # Errors
///
/// Returns [`WindowDiagnosticError::InvalidMonitorCount`] outside `1..=4`,
/// [`WindowDiagnosticError::NotEnoughDisplays`] when fewer real displays are
/// attached than `monitor_count`,
/// [`WindowDiagnosticError::Router`]/[`WindowDiagnosticError::Plan`] if the
/// (internally constructed, always valid) roster is unexpectedly rejected,
/// [`WindowDiagnosticError::MonitorResolution`] if a planned window's target
/// display stops resolving mid-run (transactional abort),
/// [`WindowDiagnosticError::Aborted`] if a definite hard failure (close
/// requested before commit, an exact display-id mismatch while bind-ready,
/// or an assigned display disappearing) is detected -- immediately, never
/// waited out to `timeout` --,
/// [`WindowDiagnosticError::Native`] if `eframe` itself fails to start (no
/// display server, graphics context failure, etc.), and
/// [`WindowDiagnosticError::UnsupportedPlatform`] outside macOS.
pub fn run_native_window_diagnostic(
    monitor_count: usize,
    timeout: Duration,
) -> Result<WindowDiagnosticReport, WindowDiagnosticError> {
    #[cfg(target_os = "macos")]
    {
        macos::run(monitor_count, timeout)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (monitor_count, timeout);
        Err(WindowDiagnosticError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::time::{Duration, Instant};

    use arcen_media::{SessionMonitorId, TopologyGeneration, MAX_MULTI_MONITOR_COUNT};

    use crate::pipeline::monitor_router::MonitorFrameRouter;
    use crate::pipeline::synthetic_multi_monitor::synthetic_frame;
    use crate::pipeline::video_decoder::DecodedVideoFrame;
    use crate::ui::multi_window_runtime::{
        evaluate_viewport_bind, monitor_index_in, viewport_builder_for, window_display_id,
        window_title_for, ActiveDisplayInfo, MonitorResolutionError, MultiWindowAbortReason,
        MultiWindowEnterAttempt, MultiWindowEnterPoll, MultiWindowPlan, ViewportBindEvaluation,
        ViewportBindObservation,
    };

    use super::{WindowDiagnosticError, WindowDiagnosticReport};

    fn color_for_frame(frame: Option<&DecodedVideoFrame>) -> egui::Color32 {
        match frame {
            Some(frame) if frame.rgba.len() >= 4 => {
                egui::Color32::from_rgb(frame.rgba[0], frame.rgba[1], frame.rgba[2])
            }
            // Unrouted/missing is a bug in this diagnostic's own setup
            // (every monitor id is routed before the app runs) -- paint it
            // in a color no `monitor_pixel_tag` output can ever produce, so
            // a regression is visually obvious rather than silently
            // blending in.
            _ => egui::Color32::from_rgb(0, 255, 0),
        }
    }

    fn observation_from_viewport_info(
        info: &egui::ViewportInfo,
        observed_display_id: Option<u32>,
    ) -> ViewportBindObservation {
        ViewportBindObservation {
            inner_rect_known: info.inner_rect.is_some(),
            fullscreen: info.fullscreen,
            close_requested: info
                .events
                .iter()
                .any(|event| matches!(event, egui::ViewportEvent::Close)),
            monitor_size_pts: info.monitor_size.map(|size| (size.x, size.y)),
            inner_rect_size_pts: info.inner_rect.map(|rect| (rect.width(), rect.height())),
            observed_display_id,
        }
    }

    struct DiagnosticApp {
        monitor_ids: Vec<SessionMonitorId>,
        router: MonitorFrameRouter,
        plan: MultiWindowPlan,
        started_at: Instant,
        timeout: Duration,
        attempt: MultiWindowEnterAttempt,
        root_fullscreen_requested: bool,
        /// The root/primary viewport's own deterministic window title (see
        /// [`window_title_for`]), used to resolve its exact observed
        /// `CGDirectDisplayID` via [`window_display_id`] the same way every
        /// additional per-monitor window is resolved.
        root_window_title: String,
        /// The exact `CGDirectDisplayID` the root viewport's `NativeOptions`
        /// was built to target -- confirmation must observe this same id,
        /// never a different (even same-sized) one.
        root_expected_cg_display_id: u32,
        /// This frame's live active display list, refreshed once per
        /// `logic()` call and read again by `ui()`'s root hard-failure check
        /// (a disappearing assigned display) so both callbacks agree on the
        /// exact same frame's list rather than each re-fetching a
        /// potentially different one.
        active_displays_this_frame: Vec<ActiveDisplayInfo>,
        /// Every expected viewport's self-reported paint acknowledgement,
        /// gathered from *inside* its own paint callback this frame (see
        /// [`super::ViewportPaintAck`]) -- the isolation proof's only input.
        /// Never populated from data computed outside a callback.
        paint_acks: Vec<super::ViewportPaintAck>,
        finished: bool,
        report: std::sync::Arc<
            std::sync::Mutex<Option<Result<WindowDiagnosticReport, WindowDiagnosticError>>>,
        >,
    }

    impl DiagnosticApp {
        /// Transactional abort: either a planned window's target display
        /// stopped resolving mid-run, or a definite hard failure (close
        /// requested before commit, an exact display-id mismatch while
        /// bind-ready, or an assigned display disappearing -- root's or any
        /// additional window's) was detected. Closes every window --
        /// including any that already confirmed -- and records the failure
        /// instead of a normal report, never presenting a partial subset of
        /// monitors, and never reopening a window afterward (`self.finished`
        /// latches permanently, mirroring
        /// `MultiWindowEnterAttempt::abort_reason`'s own latch).
        fn abort(&mut self, ctx: &egui::Context, error: WindowDiagnosticError) {
            self.finished = true;
            *self.report.lock().expect("diagnostic report mutex") = Some(Err(error));
            for viewport_id in self.plan.viewport_ids() {
                ctx.send_viewport_cmd_to(viewport_id, egui::ViewportCommand::Close);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        /// Records (replacing any stale prior entry for the same viewport)
        /// one viewport's self-reported paint acknowledgement.
        fn record_paint_ack(&mut self, ack: super::ViewportPaintAck) {
            self.paint_acks
                .retain(|existing| existing.viewport_id != ack.viewport_id);
            self.paint_acks.push(ack);
        }

        /// The plan's own `(viewport_id, session_monitor_id)` mapping, root
        /// included -- what [`super::verify_paint_isolation`] checks every
        /// paint acknowledgement against.
        fn expected_paint_plan(&self) -> Vec<(egui::ViewportId, SessionMonitorId)> {
            let mut expected = vec![(egui::ViewportId::ROOT, self.monitor_ids[0])];
            for assignment in self.plan.assignments() {
                expected.push((assignment.viewport_id, assignment.session_monitor_id));
            }
            expected
        }
    }

    impl eframe::App for DiagnosticApp {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            if self.finished {
                return;
            }

            if !self.root_fullscreen_requested {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                self.root_fullscreen_requested = true;
            }

            // Re-read the live active display list every frame (rather than
            // a snapshot frozen at startup) so a real mid-run disconnection
            // of a targeted display -- root's or any secondary's -- is
            // genuinely detected by `viewport_builder_for`/
            // `record_observation`/`ui()`'s own root check below, not just
            // in unit tests. Stored so `ui()` (called immediately after this
            // same frame, per `eframe::App::logic`'s own doc) uses this
            // exact list rather than re-fetching a possibly different one.
            self.active_displays_this_frame =
                crate::ui::multi_window_runtime::live_active_displays();
            let active_displays = self.active_displays_this_frame.clone();

            for assignment in self.plan.assignments().to_vec() {
                let monitor_id = assignment.session_monitor_id;
                let viewport_id = assignment.viewport_id;
                // Snapshot exactly the frame this monitor id currently
                // routes to *before* opening the viewport, and move that
                // same snapshot into the closure below -- the closure's own
                // paint and its self-reported acknowledgement both derive
                // from the identical routed lookup, so there is no separate
                // "planned outside" value that could drift from what the
                // callback actually consumed.
                let frame = self.router.latest_frame(monitor_id).cloned();
                let color = color_for_frame(frame.as_ref());
                let builder = match viewport_builder_for(&assignment, &active_displays) {
                    Ok(builder) => builder,
                    Err(error) => {
                        self.abort(ctx, WindowDiagnosticError::MonitorResolution(error));
                        return;
                    }
                };
                let title = window_title_for(monitor_id);
                let (observation, ack) =
                    ctx.show_viewport_immediate(viewport_id, builder, move |ui, _class| {
                        ui.painter().rect_filled(ui.max_rect(), 0.0, color);
                        let observed_display_id = window_display_id(&title);
                        let observation = observation_from_viewport_info(
                            &ui.ctx().input(|i| i.viewport().clone()),
                            observed_display_id,
                        );
                        // Acknowledged from *inside* this exact callback:
                        // which monitor it believes it is presenting
                        // (`monitor_id`, captured by this same closure, never
                        // a value from a different loop iteration) and the
                        // exact pixel tag byte it actually painted from --
                        // the isolation proof's raw material (see
                        // `super::verify_paint_isolation`). A missing frame
                        // (never expected once the router is pre-seeded, but
                        // handled defensively) uses a sentinel tag no real
                        // `monitor_pixel_tag` output for ids `1..=4` can ever
                        // produce, so a missing frame fails isolation rather
                        // than silently matching.
                        let painted_tag = frame
                            .as_ref()
                            .and_then(|frame| frame.rgba.first().copied())
                            .unwrap_or(0xFF);
                        let ack = super::ViewportPaintAck {
                            viewport_id,
                            claimed_monitor_id: monitor_id,
                            painted_tag,
                        };
                        (observation, ack)
                    });
                self.attempt
                    .record_observation(viewport_id, observation, &active_displays);
                self.record_paint_ack(ack);
            }
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            let root_monitor_id = self.monitor_ids[0];
            let root_frame = self.router.latest_frame(root_monitor_id).cloned();
            let root_color = color_for_frame(root_frame.as_ref());
            egui::CentralPanel::default().show(ui, |panel_ui| {
                panel_ui
                    .painter()
                    .rect_filled(panel_ui.max_rect(), 0.0, root_color);
            });

            if self.finished {
                return;
            }

            // Root confirmation and its paint acknowledgement are both
            // gathered from *inside* this exact paint callback --
            // `eframe::App::ui` is genuinely the root viewport's own paint
            // callback, exactly mirroring every additional window's own
            // `show_viewport_immediate` closure in `logic()` above -- never
            // assumed from `logic()`'s bookkeeping.
            let root_observed_display_id = window_display_id(&self.root_window_title);
            let root_observation = observation_from_viewport_info(
                &ui.ctx().input(|i| i.viewport().clone()),
                root_observed_display_id,
            );
            let root_target_still_active = self
                .active_displays_this_frame
                .iter()
                .any(|display| display.cg_display_id == self.root_expected_cg_display_id);
            let root_evaluation = evaluate_viewport_bind(
                root_observation,
                self.root_expected_cg_display_id,
                root_target_still_active,
            );
            if let ViewportBindEvaluation::HardFailure(kind) = root_evaluation {
                let reason = MultiWindowAbortReason {
                    viewport_id: egui::ViewportId::ROOT,
                    kind,
                };
                self.abort(ui.ctx(), WindowDiagnosticError::Aborted(reason));
                return;
            }
            let root_confirmed = matches!(root_evaluation, ViewportBindEvaluation::Confirmed);
            let root_painted_tag = root_frame
                .as_ref()
                .and_then(|frame| frame.rgba.first().copied())
                .unwrap_or(0xFF);
            self.record_paint_ack(super::ViewportPaintAck {
                viewport_id: egui::ViewportId::ROOT,
                claimed_monitor_id: root_monitor_id,
                painted_tag: root_painted_tag,
            });

            // A secondary window's hard failure (latched in `logic()`,
            // earlier this exact frame, by `MultiWindowEnterAttempt::
            // record_observation`) takes the same immediate-abort priority
            // as root's own: never wait out the timeout once it has already
            // been recorded.
            let now = self.started_at.elapsed();
            let poll = self.attempt.poll(now, self.timeout);
            if let MultiWindowEnterPoll::Aborted(reason) = poll {
                self.abort(ui.ctx(), WindowDiagnosticError::Aborted(reason));
                return;
            }

            let timed_out = now >= self.timeout;
            let secondary_done = matches!(poll, MultiWindowEnterPoll::Confirmed) || timed_out;

            if (root_confirmed || timed_out) && secondary_done {
                self.finished = true;
                let unconfirmed_viewports: Vec<egui::ViewportId> = match &poll {
                    MultiWindowEnterPoll::TimedOut { unconfirmed } => unconfirmed.clone(),
                    MultiWindowEnterPoll::Confirmed | MultiWindowEnterPoll::StillWaiting => {
                        Vec::new()
                    }
                    MultiWindowEnterPoll::Aborted(_) => {
                        unreachable!("handled above before this match is ever reached")
                    }
                };
                let mut confirmed_ids: Vec<u16> = Vec::new();
                let mut unconfirmed_ids: Vec<u16> = Vec::new();
                for (index, &monitor_id) in self.monitor_ids.iter().enumerate() {
                    let is_confirmed = if index == 0 {
                        root_confirmed
                    } else {
                        let viewport_id = self.plan.assignments()[index - 1].viewport_id;
                        !unconfirmed_viewports.contains(&viewport_id)
                    };
                    if is_confirmed {
                        confirmed_ids.push(monitor_id.get());
                    } else {
                        unconfirmed_ids.push(monitor_id.get());
                    }
                }
                let expected = self.expected_paint_plan();
                let isolation_verified =
                    super::verify_paint_isolation(&expected, &self.paint_acks).is_ok();
                *self.report.lock().expect("diagnostic report mutex") =
                    Some(Ok(WindowDiagnosticReport {
                        monitor_count: self.monitor_ids.len(),
                        confirmed_session_monitor_ids: confirmed_ids,
                        unconfirmed_session_monitor_ids: unconfirmed_ids,
                        isolation_verified,
                    }));
                for viewport_id in self.plan.viewport_ids() {
                    ui.ctx()
                        .send_viewport_cmd_to(viewport_id, egui::ViewportCommand::Close);
                }
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ui.ctx().request_repaint();
            }
        }
    }

    pub(super) fn run(
        monitor_count: usize,
        timeout: Duration,
    ) -> Result<WindowDiagnosticReport, WindowDiagnosticError> {
        if monitor_count == 0 || monitor_count > MAX_MULTI_MONITOR_COUNT {
            return Err(WindowDiagnosticError::InvalidMonitorCount(monitor_count));
        }
        // Real active displays only, in the live list's own deterministic
        // order -- never invented ids. Fails clearly, before touching any
        // native API, if fewer real displays are attached than requested
        // (task requirement: never require a real host, but never fabricate
        // hardware that is not actually there either).
        let active_displays = crate::ui::multi_window_runtime::live_active_displays();
        super::reject_if_not_enough_displays(monitor_count, active_displays.len())?;
        let monitor_ids: Vec<SessionMonitorId> = (1..=monitor_count as u16)
            .map(|wire_id| {
                SessionMonitorId::new(wire_id)
                    .unwrap_or_else(|_| unreachable!("{wire_id} is in 1..=monitor_count, never 0"))
            })
            .collect();
        let generation =
            TopologyGeneration::new(1).unwrap_or_else(|_| unreachable!("1 is nonzero"));
        let mut router = MonitorFrameRouter::new(generation, &monitor_ids)
            .map_err(WindowDiagnosticError::Router)?;
        for &monitor_id in &monitor_ids {
            router
                .route_decoded_frame(generation, monitor_id, synthetic_frame(monitor_id, 0))
                .unwrap_or_else(|error| {
                    unreachable!(
                        "routing session monitor id {} into a router just built for exactly \
                         this roster and generation cannot be rejected: {error}",
                        monitor_id.get()
                    )
                });
        }

        // The first `monitor_count` real active displays, in the live
        // list's own order, become this run's roster: `monitor_ids[0]`
        // (the root/primary) is `active_displays[0]`, and so on. This
        // proves the real window-opening/fullscreen/paint/isolation
        // mechanism against actually attached hardware rather than any
        // invented id (task 5's "no cross-routing" proof still holds
        // regardless of how many real displays are exercised).
        let cg_display_ids: Vec<u32> = active_displays[..monitor_count]
            .iter()
            .map(|display| display.cg_display_id)
            .collect();
        let plan = MultiWindowPlan::build(&monitor_ids, &cg_display_ids)
            .map_err(WindowDiagnosticError::Plan)?;

        // The root viewport must explicitly target the primary display too
        // (never an unpinned/default window that could land anywhere) so
        // its confirmation can use the exact same identity check every
        // additional window uses.
        let root_cg_display_id = cg_display_ids[0];
        let root_window_title = window_title_for(monitor_ids[0]);
        let live_active_display_ids: Vec<u32> = active_displays
            .iter()
            .map(|display| display.cg_display_id)
            .collect();
        let root_monitor_index = monitor_index_in(&live_active_display_ids, root_cg_display_id)
            .ok_or(WindowDiagnosticError::MonitorResolution(
                MonitorResolutionError::DisplayNotActive(root_cg_display_id),
            ))?;

        let report_slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let report_slot_for_app = report_slot.clone();

        let mut options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title(root_window_title.clone())
                .with_decorations(false)
                .with_resizable(false)
                .with_monitor(root_monitor_index),
            persist_window: false,
            ..Default::default()
        };
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder
                .with_activation_policy(ActivationPolicy::Accessory)
                .with_default_menu(false);
        }));

        let attempt = MultiWindowEnterAttempt::new(plan.clone(), Duration::ZERO);
        eframe::run_native(
            "Arcen Deck Multi-Window Diagnostic",
            options,
            Box::new(move |_cc| {
                Ok(Box::new(DiagnosticApp {
                    monitor_ids,
                    router,
                    plan,
                    started_at: Instant::now(),
                    timeout,
                    attempt,
                    root_fullscreen_requested: false,
                    root_window_title,
                    root_expected_cg_display_id: root_cg_display_id,
                    active_displays_this_frame: Vec::new(),
                    paint_acks: Vec::new(),
                    finished: false,
                    report: report_slot_for_app,
                }))
            }),
        )
        .map_err(WindowDiagnosticError::Native)?;

        let outcome = report_slot.lock().expect("diagnostic report mutex").take();

        match outcome {
            Some(Ok(report)) => Ok(report),
            Some(Err(error)) => Err(error),
            None => Ok(WindowDiagnosticReport {
                monitor_count,
                confirmed_session_monitor_ids: Vec::new(),
                unconfirmed_session_monitor_ids: (1..=monitor_count as u16).collect(),
                isolation_verified: false,
            }),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::ui::multi_window_runtime::MonitorWindowAssignment;

        fn viewport_info(fullscreen: bool, inner_rect_known: bool) -> egui::ViewportInfo {
            let mut info = egui::ViewportInfo {
                fullscreen: Some(fullscreen),
                ..Default::default()
            };
            if inner_rect_known {
                info.inner_rect = Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1920.0, 1080.0),
                ));
            }
            info
        }

        #[test]
        fn observation_from_viewport_info_carries_the_resolved_display_id_through() {
            let info = viewport_info(true, true);
            let observation = observation_from_viewport_info(&info, Some(42));
            assert_eq!(observation.observed_display_id, Some(42));
            assert!(observation.inner_rect_known);
            assert_eq!(observation.fullscreen, Some(true));
        }

        #[test]
        fn observation_from_viewport_info_carries_a_missing_display_id_through() {
            let info = viewport_info(true, true);
            let observation = observation_from_viewport_info(&info, None);
            assert_eq!(observation.observed_display_id, None);
        }

        #[test]
        fn root_confirmation_matches_the_exact_expected_display_only() {
            // "Root confirmation uses the same exact-id check": build the
            // exact composition `DiagnosticApp::ui` uses for the root
            // viewport and prove a correct id confirms while a same-sized
            // swapped id does not, without needing a real event loop.
            let info = viewport_info(true, true);
            let confirming = observation_from_viewport_info(&info, Some(10));
            assert_eq!(
                evaluate_viewport_bind(confirming, 10, true),
                ViewportBindEvaluation::Confirmed
            );

            let swapped = observation_from_viewport_info(&info, Some(20));
            assert_eq!(
                evaluate_viewport_bind(swapped, 10, true),
                ViewportBindEvaluation::HardFailure(
                    crate::ui::multi_window_runtime::HardFailureKind::WrongDisplay {
                        expected_cg_display_id: 10,
                        observed_cg_display_id: 20,
                    }
                )
            );

            // Identity simply not observable yet stays ordinary pending
            // state, never a hard failure -- distinct from a *definite*
            // wrong id above.
            let missing_identity = observation_from_viewport_info(&info, None);
            assert_eq!(
                evaluate_viewport_bind(missing_identity, 10, true),
                ViewportBindEvaluation::Pending
            );
        }

        #[test]
        fn root_hard_failure_close_requested_before_commit_aborts_immediately() {
            // Finding 2: `close_requested` must be a definite, immediate
            // hard failure, never merely un-confirmed until a timeout.
            let mut info = viewport_info(true, true);
            info.events.push(egui::ViewportEvent::Close);
            let observation = observation_from_viewport_info(&info, Some(10));
            assert_eq!(
                evaluate_viewport_bind(observation, 10, true),
                ViewportBindEvaluation::HardFailure(
                    crate::ui::multi_window_runtime::HardFailureKind::CloseRequested
                )
            );
        }

        #[test]
        fn root_hard_failure_disappearing_display_aborts_immediately() {
            // Finding 2: the root's own assigned display disappearing (not
            // in the live active list) must also abort immediately, exactly
            // like any additional window's own assigned display doing so.
            let info = viewport_info(true, true);
            let observation = observation_from_viewport_info(&info, Some(10));
            assert_eq!(
                evaluate_viewport_bind(observation, 10, false),
                ViewportBindEvaluation::HardFailure(
                    crate::ui::multi_window_runtime::HardFailureKind::DisplayDisappeared {
                        cg_display_id: 10
                    }
                )
            );
        }

        #[test]
        fn verify_paint_isolation_accepts_correctly_claimed_and_painted_acks() {
            let expected = vec![
                (egui::ViewportId::ROOT, sid(1)),
                (MonitorWindowAssignment::viewport_id_for(sid(2)), sid(2)),
            ];
            let acks = vec![
                super::super::ViewportPaintAck {
                    viewport_id: egui::ViewportId::ROOT,
                    claimed_monitor_id: sid(1),
                    painted_tag: crate::pipeline::synthetic_multi_monitor::monitor_pixel_tag(sid(
                        1,
                    )),
                },
                super::super::ViewportPaintAck {
                    viewport_id: MonitorWindowAssignment::viewport_id_for(sid(2)),
                    claimed_monitor_id: sid(2),
                    painted_tag: crate::pipeline::synthetic_multi_monitor::monitor_pixel_tag(sid(
                        2,
                    )),
                },
            ];
            assert!(super::super::verify_paint_isolation(&expected, &acks).is_ok());
        }

        #[test]
        fn verify_paint_isolation_catches_a_swapped_callback_claim() {
            // The core "swapped callback routing" regression: viewport 2's
            // own callback claims monitor 1 (root's monitor) instead of its
            // own assigned monitor 2 -- this must be caught by comparing
            // against the plan's own expected mapping, not merely by
            // checking pairwise tag distinctness.
            let root_id = egui::ViewportId::ROOT;
            let secondary_id = MonitorWindowAssignment::viewport_id_for(sid(2));
            let expected = vec![(root_id, sid(1)), (secondary_id, sid(2))];
            let acks = vec![
                super::super::ViewportPaintAck {
                    viewport_id: root_id,
                    claimed_monitor_id: sid(2),
                    painted_tag: crate::pipeline::synthetic_multi_monitor::monitor_pixel_tag(sid(
                        2,
                    )),
                },
                super::super::ViewportPaintAck {
                    viewport_id: secondary_id,
                    claimed_monitor_id: sid(1),
                    painted_tag: crate::pipeline::synthetic_multi_monitor::monitor_pixel_tag(sid(
                        1,
                    )),
                },
            ];
            let error = super::super::verify_paint_isolation(&expected, &acks).unwrap_err();
            assert_eq!(
                error,
                super::super::PaintIsolationError::WrongMonitorClaim {
                    viewport_id: root_id,
                    expected_monitor_id: sid(1),
                    claimed_monitor_id: sid(2),
                }
            );
        }

        #[test]
        fn verify_paint_isolation_catches_a_claim_whose_painted_tag_does_not_match() {
            // The claim itself is correct, but the tag actually painted
            // belongs to a different monitor -- proves the isolation check
            // does not merely trust a self-reported id, it cross-checks the
            // real painted data too.
            let expected = vec![(egui::ViewportId::ROOT, sid(1))];
            let acks = vec![super::super::ViewportPaintAck {
                viewport_id: egui::ViewportId::ROOT,
                claimed_monitor_id: sid(1),
                painted_tag: crate::pipeline::synthetic_multi_monitor::monitor_pixel_tag(sid(2)),
            }];
            let error = super::super::verify_paint_isolation(&expected, &acks).unwrap_err();
            assert_eq!(
                error,
                super::super::PaintIsolationError::PaintedTagDoesNotMatchClaim {
                    viewport_id: egui::ViewportId::ROOT,
                    claimed_monitor_id: sid(1),
                    painted_tag: crate::pipeline::synthetic_multi_monitor::monitor_pixel_tag(sid(
                        2
                    )),
                }
            );
        }

        #[test]
        fn verify_paint_isolation_catches_a_missing_ack() {
            let expected = vec![(egui::ViewportId::ROOT, sid(1))];
            let error = super::super::verify_paint_isolation(&expected, &[]).unwrap_err();
            assert_eq!(
                error,
                super::super::PaintIsolationError::MissingAck(egui::ViewportId::ROOT)
            );
        }

        fn sid(value: u16) -> SessionMonitorId {
            SessionMonitorId::new(value).expect("test session monitor id must be nonzero")
        }

        #[test]
        fn window_title_for_matches_the_diagnostics_own_root_title_format() {
            let sid = SessionMonitorId::new(1).expect("nonzero");
            assert_eq!(window_title_for(sid), "Arcen Deck — Display 1");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_display_includes_every_field() {
        let report = WindowDiagnosticReport {
            monitor_count: 2,
            confirmed_session_monitor_ids: vec![1, 2],
            unconfirmed_session_monitor_ids: vec![],
            isolation_verified: true,
        };
        let text = report.to_string();
        assert!(text.contains("monitors=2"));
        assert!(text.contains("isolation_verified=true"));
    }

    // These two cases return before any native window is ever opened (see
    // `macos::run`'s leading validation), so they are safe to execute in an
    // automated/headless test run without touching the real WindowServer.
    #[test]
    fn invalid_monitor_count_zero_is_rejected_before_touching_any_native_api() {
        let error =
            run_native_window_diagnostic(0, Duration::from_millis(1)).expect_err("must reject 0");
        assert!(matches!(
            error,
            WindowDiagnosticError::InvalidMonitorCount(0)
        ));
    }

    #[test]
    fn invalid_monitor_count_above_max_is_rejected_before_touching_any_native_api() {
        let error =
            run_native_window_diagnostic(5, Duration::from_millis(1)).expect_err("must reject 5");
        assert!(matches!(
            error,
            WindowDiagnosticError::InvalidMonitorCount(5)
        ));
    }

    #[test]
    fn not_enough_displays_is_rejected_before_touching_any_native_api() {
        // Finding 1: the diagnostic must never invent synthetic display ids
        // to make up a requested count that exceeds what is actually
        // attached; it must fail clearly instead.
        let error = reject_if_not_enough_displays(4, 1).expect_err("must reject 4 with only 1");
        assert_eq!(
            error.to_string(),
            "requested 4 monitors but only 1 display(s) are attached"
        );
        assert!(matches!(
            error,
            WindowDiagnosticError::NotEnoughDisplays {
                requested: 4,
                available: 1,
            }
        ));
    }

    #[test]
    fn enough_displays_is_accepted() {
        assert!(reject_if_not_enough_displays(1, 1).is_ok());
        assert!(reject_if_not_enough_displays(2, 4).is_ok());
        assert!(reject_if_not_enough_displays(4, 4).is_ok());
    }

    #[test]
    fn window_diagnostic_error_display_covers_every_variant() {
        assert_eq!(
            WindowDiagnosticError::InvalidMonitorCount(0).to_string(),
            "invalid diagnostic monitor count: 0"
        );
        assert_eq!(
            WindowDiagnosticError::NotEnoughDisplays {
                requested: 2,
                available: 1
            }
            .to_string(),
            "requested 2 monitors but only 1 display(s) are attached"
        );
        assert_eq!(
            WindowDiagnosticError::UnsupportedPlatform.to_string(),
            "the native multi-window diagnostic only opens real windows on macOS"
        );
    }
}
