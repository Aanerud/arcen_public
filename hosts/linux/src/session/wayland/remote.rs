//! Remote-monitor outputs: a session's virtual outputs as Mutter
//! `ScreenCast.RecordVirtual` monitors (Mutter 49 and later, see
//! [`arcen_session::wayland_outputs`]).
//!
//! The paired `RemoteDesktop`/`ScreenCast` session records one virtual
//! stream per output. Each monitor comes into existence when a consumer
//! negotiates the stream's size and refresh
//! ([`arcen_capenc::materialise_virtual_output`]), persists when that
//! consumer leaves, and is resized in place by a later negotiation; the
//! monitors are placed with `DisplayConfig.ApplyMonitorsConfig` and removed
//! with their session. Every step is checked against what `DisplayConfig`
//! reports before it counts.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use arcen_media::video::capture_handoff::PipeWireCaptureHandoff;
use arcen_session::wayland_outputs::{
    logical_positions, verify_placement, LayoutMode, LogicalLayoutError, PlacedOutput,
    PlacementMismatch, ScaledOutput,
};
use arcen_session::wayland_session::{
    verify_virtual_outputs, ObservedMonitor, OutputVerifyError, VerifiedOutput,
    VirtualOutputRequest,
};
use futures_util::StreamExt;
use thiserror::Error;
use zbus::Connection;

use super::mutter::{self, MutterError, MutterSessions};
use super::{ReadyOutput, ReadyStream, StreamCursorMode, INITIAL_OUTPUT_GENERATION};
use crate::logging::target;

/// How long one negotiation may take.
const MATERIALISE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long Mutter may take to report a created, resized or placed monitor.
const MONITOR_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);

/// A remote-monitor failure.
#[derive(Debug, Error)]
pub enum RemoteOutputError {
    #[error("compositor D-Bus failed: {0}")]
    Bus(#[from] MutterError),
    #[error("output {index} could not be negotiated: {reason}")]
    Materialise { index: usize, reason: String },
    #[error("output {index}: no {width}x{height} monitor appeared")]
    MonitorMissing {
        index: usize,
        width: u32,
        height: u32,
    },
    #[error("outputs do not match the request: {0}")]
    Geometry(#[from] OutputVerifyError),
    #[error("outputs are not placed as requested: {0}")]
    Placement(#[from] PlacementMismatch),
    #[error("the layout has no equivalent in the compositor's logical layout mode: {0}")]
    LogicalLayout(#[from] LogicalLayoutError),
}

/// The layout the placement keeper maintains; `None` pauses it.
pub type DesiredPlacement = Option<Vec<(String, PlacedOutput)>>;

/// The live outputs of a session and the paired sessions that record them
/// (in the startup-monitor mode, the `RecordMonitor` streams of the
/// compositor's own monitors).
pub struct RemoteSet {
    /// Unique per set, so a waiter can tell a replaced set from a closed one.
    pub id: u64,
    /// The paired sessions whose virtual streams own the monitors.
    pub sessions: MutterSessions,
    /// Requested placement of each output, in record order.
    pub placed: Vec<PlacedOutput>,
    /// Connector of each output, in record order.
    pub connectors: Vec<String>,
    /// How every stream of the set reports the cursor; a set recorded
    /// again keeps it.
    pub cursor_mode: StreamCursorMode,
}

static NEXT_SET_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl RemoteSet {
    /// A set of `sessions` recording `placed` outputs on `connectors`.
    #[must_use]
    pub fn new(
        sessions: MutterSessions,
        placed: Vec<PlacedOutput>,
        connectors: Vec<String>,
        cursor_mode: StreamCursorMode,
    ) -> Self {
        Self {
            id: NEXT_SET_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            sessions,
            placed,
            connectors,
            cursor_mode,
        }
    }

    /// The outputs in the agent's ready shape.
    #[must_use]
    pub fn ready_outputs(&self) -> Vec<ReadyOutput> {
        self.connectors
            .iter()
            .zip(&self.placed)
            .map(|(connector, placed)| ReadyOutput {
                connector: connector.clone(),
                width: placed.request.width(),
                height: placed.request.height(),
                refresh_hz: placed.request.refresh_hz(),
            })
            .collect()
    }

    /// The streams in the agent's ready shape.
    #[must_use]
    pub fn ready_streams(&self) -> Vec<ReadyStream> {
        self.sessions.ready_streams()
    }

    /// (connector, placement) pairs, in record order.
    #[must_use]
    pub fn placements(&self) -> Vec<(String, PlacedOutput)> {
        self.connectors
            .iter()
            .cloned()
            .zip(self.placed.iter().copied())
            .collect()
    }
}

/// Negotiates `request` on `node`, creating or resizing its monitor.
async fn materialise(
    runtime_dir: &Path,
    index: usize,
    node: u32,
    request: VirtualOutputRequest,
    cursor_mode: StreamCursorMode,
) -> Result<(), RemoteOutputError> {
    let failed = |reason: String| RemoteOutputError::Materialise { index, reason };
    let handoff = PipeWireCaptureHandoff::new(
        node,
        &mutter::virtual_placeholder(index),
        INITIAL_OUTPUT_GENERATION,
        (request.width(), request.height()),
        (0, 0),
        cursor_mode.screencast(),
        runtime_dir,
    )
    .map_err(|error| failed(error.to_string()))?;
    // A dedicated thread: the negotiation blocks on PipeWire's own loop and
    // bounds itself with MATERIALISE_TIMEOUT.
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("arcen-materialise".to_owned())
        .spawn(move || {
            let _ = sender.send(arcen_capenc::materialise_virtual_output(
                &handoff,
                request.width(),
                request.height(),
                request.refresh_hz(),
                MATERIALISE_TIMEOUT,
            ));
        })
        .map_err(|error| failed(error.to_string()))?;
    let outcome = receiver
        .await
        .map_err(|error| failed(error.to_string()))?
        .map_err(failed)?;
    tracing::info!(
        target: target::SESSION,
        index,
        node,
        width = outcome.width,
        height = outcome.height,
        max_framerate = ?outcome.max_framerate,
        buffer_seen = outcome.buffer_seen,
        "virtual output negotiated"
    );
    Ok(())
}

fn mode_matches(monitor: &mutter::DisplayMonitor, request: VirtualOutputRequest) -> bool {
    monitor
        .current
        .as_ref()
        .is_some_and(|(_, width, height, _)| {
            (*width, *height) == (request.width(), request.height())
        })
}

/// Waits for a remote monitor not in `known` that runs `request`.
async fn new_monitor(
    connection: &Connection,
    known: &BTreeSet<String>,
    index: usize,
    request: VirtualOutputRequest,
) -> Result<String, RemoteOutputError> {
    let deadline = tokio::time::Instant::now() + MONITOR_TIMEOUT;
    loop {
        let state = mutter::display_state(connection).await?;
        if let Some(monitor) = state.monitors.iter().find(|monitor| {
            monitor.remote && !known.contains(&monitor.connector) && mode_matches(monitor, request)
        }) {
            return Ok(monitor.connector.clone());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RemoteOutputError::MonitorMissing {
                index,
                width: request.width(),
                height: request.height(),
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Waits until `connector` runs `request`.
async fn monitor_mode(
    connection: &Connection,
    connector: &str,
    index: usize,
    request: VirtualOutputRequest,
) -> Result<(), RemoteOutputError> {
    let deadline = tokio::time::Instant::now() + MONITOR_TIMEOUT;
    loop {
        let state = mutter::display_state(connection).await?;
        if state
            .monitors
            .iter()
            .any(|monitor| monitor.connector == connector && mode_matches(monitor, request))
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RemoteOutputError::MonitorMissing {
                index,
                width: request.width(),
                height: request.height(),
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Places exactly `placements` unless they already are, then waits until
/// `DisplayConfig` reports them; every other monitor ends up disabled.
///
/// # Errors
///
/// [`RemoteOutputError`] when Mutter refuses or does not apply the layout.
pub async fn place(
    connection: &Connection,
    placements: &[(String, PlacedOutput)],
) -> Result<(), RemoteOutputError> {
    let deadline = tokio::time::Instant::now() + MONITOR_TIMEOUT;
    let mut applied_serial = None;
    loop {
        let state = mutter::display_state(connection).await?;
        let (targets, scales) = layout_targets(&state, placements)?;
        let verdict = verify_placement(&targets, &state.placements).and_then(|()| {
            targets
                .iter()
                .zip(&scales)
                .find_map(|((connector, _), scale)| {
                    let observed = state
                        .scales
                        .iter()
                        .find(|(observed, _)| observed == connector)
                        .map(|(_, observed)| *observed)?;
                    (thousandths(observed) != thousandths(*scale)).then(|| {
                        PlacementMismatch::Scaled {
                            connector: connector.clone(),
                            expected: thousandths(*scale),
                            observed: thousandths(observed),
                        }
                    })
                })
                .map_or(Ok(()), Err)
        });
        if verdict.is_ok() {
            return Ok(());
        }
        if applied_serial != Some(state.serial) {
            let modes = targets
                .iter()
                .zip(&scales)
                .map(|((connector, placed), scale)| {
                    let mode = state
                        .monitors
                        .iter()
                        .find(|monitor| &monitor.connector == connector)
                        .and_then(|monitor| monitor.current.as_ref())
                        .map(|(mode, ..)| mode.clone())
                        .ok_or_else(|| PlacementMismatch::Missing(connector.clone()))?;
                    Ok((connector.clone(), mode, placed.x, placed.y, *scale))
                })
                .collect::<Result<Vec<_>, PlacementMismatch>>()?;
            // Mutter refuses a configuration built on an older serial
            // ("stale monitor state") when a monitor changed meanwhile; read
            // the state again and retry until the deadline.
            match mutter::apply_layout(connection, state.serial, &modes).await {
                Ok(()) => applied_serial = Some(state.serial),
                Err(error) => {
                    tracing::info!(target: target::SESSION, %error, serial = state.serial, "output placement refused; retrying on fresh monitor state");
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(verdict.err().map_or_else(
                || RemoteOutputError::Placement(PlacementMismatch::Missing(String::new())),
                RemoteOutputError::Placement,
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// A UI scale in thousandths, for comparing what was applied.
fn thousandths(scale: f64) -> u32 {
    // UI scales are small positive numbers.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let value = (scale * 1000.0).round() as u32;
    value
}

/// Placements in the compositor's layout mode, and each output's UI scale.
type LayoutTargets = (Vec<(String, PlacedOutput)>, Vec<f64>);

/// What `placements` (desktop pixels) asks of the compositor in `state`:
/// each output's UI scale, and its position in the compositor's layout
/// mode — the desktop position in the physical mode, the shared
/// [`logical_positions`] conversion in the logical one
/// (`scale-monitor-framebuffer`), where an output spans its size over its
/// scale.
///
/// Scales that leave the layout without a logical equivalent fall back to
/// 1 for every output.
///
/// # Errors
///
/// [`RemoteOutputError::LogicalLayout`] when even the unscaled layout has
/// none (an output touches no other).
fn layout_targets(
    state: &mutter::DisplayState,
    placements: &[(String, PlacedOutput)],
) -> Result<LayoutTargets, RemoteOutputError> {
    let scales = placements
        .iter()
        .map(|(connector, placed)| output_scale(state, connector, placed))
        .collect::<Vec<_>>();
    if state.layout == LayoutMode::Physical {
        return Ok((placements.to_vec(), scales));
    }
    let scaled = |scales: &[f64]| {
        logical_positions(
            &placements
                .iter()
                .zip(scales)
                .map(|((_, placed), scale)| ScaledOutput {
                    x: placed.x,
                    y: placed.y,
                    width: placed.request.width(),
                    height: placed.request.height(),
                    scale: *scale,
                })
                .collect::<Vec<_>>(),
        )
    };
    // Scales that tear the layout apart in logical pixels give way to an
    // unscaled layout, which always has one (it is the desktop's).
    let (positions, scales) = match scaled(&scales) {
        Ok(positions) => (positions, scales),
        Err(error) => {
            tracing::warn!(target: target::SESSION, %error, "output UI scales do not fit the logical layout; placing the outputs unscaled");
            let unscaled = vec![1.0; placements.len()];
            (scaled(&unscaled)?, unscaled)
        }
    };
    let targets = placements
        .iter()
        .zip(positions)
        .map(|((connector, placed), (x, y))| (connector.clone(), PlacedOutput { x, y, ..*placed }))
        .collect();
    Ok((targets, scales))
}

/// The UI scale `placed` gets on `connector`: chosen from the client
/// display's density among the scales the monitor's current mode supports
/// (`arcen_outputs::choose_output_scale`), 1 when nothing is known.
fn output_scale(state: &mutter::DisplayState, connector: &str, placed: &PlacedOutput) -> f64 {
    // The physical layout mode scales by whole numbers only.
    let supported = state
        .monitors
        .iter()
        .find(|monitor| monitor.connector == connector)
        .map(|monitor| {
            monitor
                .supported_scales
                .iter()
                .copied()
                .filter(|scale| {
                    state.layout == LayoutMode::Logical || (scale - scale.round()).abs() < 1e-6
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let choice = arcen_outputs::choose_output_scale(
        &arcen_outputs::OutputScaleFacts {
            width_px: placed.request.width(),
            height_px: placed.request.height(),
            logical_width: (placed.density.logical_width > 0)
                .then_some(placed.density.logical_width),
            width_mm: placed.density.width_mm,
            height_mm: placed.density.height_mm,
        },
        &supported,
    );
    if choice.scale > 1.0 {
        tracing::debug!(
            target: target::SESSION,
            connector,
            scale = choice.scale,
            ideal = choice.ideal,
            basis = choice.basis.token(),
            "virtual output UI scale chosen"
        );
    }
    choice.scale
}

/// Creates a new paired session with one virtual stream per output,
/// negotiates each output into existence in order, names it after the
/// monitor that appeared and places them all.
///
/// # Errors
///
/// [`RemoteOutputError`]; the new sessions are stopped first.
pub async fn establish(
    connection: &Connection,
    runtime_dir: &Path,
    placed: &[PlacedOutput],
    cursor_mode: StreamCursorMode,
) -> Result<RemoteSet, RemoteOutputError> {
    let mut sessions = MutterSessions::start_virtual(
        connection,
        placed.len(),
        cursor_mode.screencast().mutter_value(),
    )
    .await?;
    match name_outputs(connection, runtime_dir, &mut sessions, placed, cursor_mode).await {
        Ok(connectors) => Ok(RemoteSet::new(
            sessions,
            placed.to_vec(),
            connectors,
            cursor_mode,
        )),
        Err(error) => {
            sessions.stop().await;
            Err(error)
        }
    }
}

async fn name_outputs(
    connection: &Connection,
    runtime_dir: &Path,
    sessions: &mut MutterSessions,
    placed: &[PlacedOutput],
    cursor_mode: StreamCursorMode,
) -> Result<Vec<String>, RemoteOutputError> {
    let nodes = sessions.node_ids();
    let mut connectors = Vec::with_capacity(placed.len());
    for (index, (node, output)) in nodes.iter().zip(placed).enumerate() {
        let known = mutter::display_state(connection)
            .await?
            .monitors
            .into_iter()
            .map(|monitor| monitor.connector)
            .collect::<BTreeSet<_>>();
        materialise(runtime_dir, index, *node, output.request, cursor_mode).await?;
        let connector = new_monitor(connection, &known, index, output.request).await?;
        sessions.assign(
            index,
            &connector,
            (output.request.width(), output.request.height()),
            (output.x, output.y),
        );
        connectors.push(connector);
    }
    let placements = connectors
        .iter()
        .cloned()
        .zip(placed.iter().copied())
        .collect::<Vec<_>>();
    place(connection, &placements).await?;
    sessions.refresh_input_geometry().await;
    Ok(connectors)
}

/// The compositor steps a size or count change is made of, over a set's
/// bookkeeping (`placed`, `connectors`, the recorded streams). The live
/// implementation drives Mutter; tests inject failures.
trait OutputOps {
    fn placed(&self) -> &[PlacedOutput];
    fn placed_mut(&mut self) -> &mut Vec<PlacedOutput>;
    fn connectors(&self) -> &[String];
    fn connectors_mut(&mut self) -> &mut Vec<String>;
    /// Renegotiates output `index`'s stream and waits for its monitor to run
    /// `request`.
    async fn renegotiate(
        &mut self,
        index: usize,
        request: VirtualOutputRequest,
    ) -> Result<(), RemoteOutputError>;
    /// Records one more stream, negotiates it to `request` and returns the
    /// connector of the monitor that appeared. On failure no stream is left
    /// recorded.
    async fn add(
        &mut self,
        index: usize,
        request: VirtualOutputRequest,
    ) -> Result<String, RemoteOutputError>;
    /// Stops and forgets the last recorded stream (it is forgotten even when
    /// the compositor refuses the stop).
    async fn stop_last(&mut self) -> Result<(), RemoteOutputError>;
    async fn place(
        &mut self,
        placements: &[(String, PlacedOutput)],
    ) -> Result<(), RemoteOutputError>;
    /// Names stream `index` after its monitor, size and place.
    fn assign(&mut self, index: usize, connector: &str, placed: &PlacedOutput);
}

struct LiveOps<'set> {
    connection: &'set Connection,
    runtime_dir: &'set Path,
    set: &'set mut RemoteSet,
}

impl OutputOps for LiveOps<'_> {
    fn placed(&self) -> &[PlacedOutput] {
        &self.set.placed
    }

    fn placed_mut(&mut self) -> &mut Vec<PlacedOutput> {
        &mut self.set.placed
    }

    fn connectors(&self) -> &[String] {
        &self.set.connectors
    }

    fn connectors_mut(&mut self) -> &mut Vec<String> {
        &mut self.set.connectors
    }

    async fn renegotiate(
        &mut self,
        index: usize,
        request: VirtualOutputRequest,
    ) -> Result<(), RemoteOutputError> {
        let (Some(node), Some(connector)) = (
            self.set.sessions.node_ids().get(index).copied(),
            self.set.connectors.get(index).cloned(),
        ) else {
            return Err(RemoteOutputError::MonitorMissing {
                index,
                width: request.width(),
                height: request.height(),
            });
        };
        materialise(self.runtime_dir, index, node, request, self.set.cursor_mode).await?;
        monitor_mode(self.connection, &connector, index, request).await
    }

    async fn add(
        &mut self,
        index: usize,
        request: VirtualOutputRequest,
    ) -> Result<String, RemoteOutputError> {
        let known = mutter::display_state(self.connection)
            .await?
            .monitors
            .into_iter()
            .map(|monitor| monitor.connector)
            .collect::<BTreeSet<_>>();
        let node = self
            .set
            .sessions
            .add_virtual(self.set.cursor_mode.screencast().mutter_value())
            .await?;
        let appeared = async {
            materialise(self.runtime_dir, index, node, request, self.set.cursor_mode).await?;
            new_monitor(self.connection, &known, index, request).await
        }
        .await;
        if appeared.is_err() {
            if let Err(error) = self.set.sessions.stop_last_stream().await {
                tracing::warn!(target: target::SESSION, %error, "an added output could not be stopped again");
            }
        }
        appeared
    }

    async fn stop_last(&mut self) -> Result<(), RemoteOutputError> {
        Ok(self.set.sessions.stop_last_stream().await?)
    }

    async fn place(
        &mut self,
        placements: &[(String, PlacedOutput)],
    ) -> Result<(), RemoteOutputError> {
        place(self.connection, placements).await?;
        self.set.sessions.refresh_input_geometry().await;
        Ok(())
    }

    fn assign(&mut self, index: usize, connector: &str, placed: &PlacedOutput) {
        self.set.sessions.assign(
            index,
            connector,
            (placed.request.width(), placed.request.height()),
            (placed.x, placed.y),
        );
    }
}

/// Places `requested` on the set's connectors, names the streams and makes
/// `requested` the set's placement.
async fn place_all<O: OutputOps>(
    ops: &mut O,
    requested: &[PlacedOutput],
) -> Result<(), RemoteOutputError> {
    let placements = ops
        .connectors()
        .iter()
        .cloned()
        .zip(requested.iter().copied())
        .collect::<Vec<_>>();
    ops.place(&placements).await?;
    for (index, (connector, output)) in placements.iter().enumerate() {
        ops.assign(index, connector, output);
    }
    *ops.placed_mut() = requested.to_vec();
    Ok(())
}

/// Stops the last output; its stream, connector and placement go together
/// whether or not the compositor accepted the stop.
async fn drop_last<O: OutputOps>(ops: &mut O) -> Result<(), RemoteOutputError> {
    let stopped = ops.stop_last().await;
    ops.connectors_mut().pop();
    ops.placed_mut().pop();
    stopped
}

async fn resize_with<O: OutputOps>(
    ops: &mut O,
    requested: &[PlacedOutput],
    resized: &[usize],
) -> Result<(), RemoteOutputError> {
    for &index in resized {
        if let Some(output) = requested.get(index) {
            ops.renegotiate(index, output.request).await?;
        }
    }
    place_all(ops, requested).await
}

/// Grows to `requested`: renegotiates the kept outputs (every one when
/// `force`, else those whose size changes), adds the rest and places them
/// all. On failure the added outputs are stopped again; `placed` keeps the
/// previous placement.
async fn grow_with<O: OutputOps>(
    ops: &mut O,
    requested: &[PlacedOutput],
    force: bool,
) -> Result<(), RemoteOutputError> {
    let kept = ops.connectors().len().min(requested.len());
    let mut added = 0;
    let result = async {
        for (index, output) in requested.iter().enumerate().take(kept) {
            let changed = ops
                .placed()
                .get(index)
                .is_none_or(|previous| previous.request != output.request);
            if force || changed {
                ops.renegotiate(index, output.request).await?;
            }
        }
        for (index, output) in requested.iter().enumerate().skip(kept) {
            let connector = ops.add(index, output.request).await?;
            ops.connectors_mut().push(connector);
            added += 1;
        }
        place_all(ops, requested).await
    }
    .await;
    if result.is_err() {
        for _ in 0..added {
            if let Err(error) = ops.stop_last().await {
                tracing::warn!(target: target::SESSION, %error, "an added output could not be stopped again");
            }
            ops.connectors_mut().pop();
        }
    }
    result
}

async fn shrink_with<O: OutputOps>(
    ops: &mut O,
    requested: &[PlacedOutput],
) -> Result<(), RemoteOutputError> {
    while ops.connectors().len() > requested.len() {
        drop_last(ops).await?;
    }
    let resized = ops
        .placed()
        .iter()
        .zip(requested)
        .enumerate()
        .filter(|(_, (previous, output))| previous.request != output.request)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    resize_with(ops, requested, &resized).await
}

/// Returns the set to `previous` whatever a failed change left behind:
/// stops outputs beyond it, renegotiates every kept output to its previous
/// size (the compositor may run a size `placed` no longer records), adds
/// the missing ones and places them all.
async fn restore_with<O: OutputOps>(
    ops: &mut O,
    previous: &[PlacedOutput],
) -> Result<(), RemoteOutputError> {
    let mut stop_failure = None;
    while ops.connectors().len() > previous.len() {
        if let Err(error) = drop_last(ops).await {
            tracing::warn!(target: target::SESSION, %error, "an output could not be stopped while restoring");
            stop_failure.get_or_insert(error);
        }
    }
    grow_with(ops, previous, true).await?;
    stop_failure.map_or(Ok(()), Err)
}

/// Resizes the outputs at `resized` in place to `requested`, then places
/// every output; `set` follows only once all of it succeeded.
///
/// # Errors
///
/// [`RemoteOutputError`]; `set` is unchanged, the compositor may be partly
/// changed (the caller restores with [`restore_outputs`]).
pub async fn resize_in_place(
    connection: &Connection,
    runtime_dir: &Path,
    set: &mut RemoteSet,
    requested: &[PlacedOutput],
    resized: &[usize],
) -> Result<(), RemoteOutputError> {
    let mut ops = LiveOps {
        connection,
        runtime_dir,
        set,
    };
    resize_with(&mut ops, requested, resized).await
}

/// Grows `set` to `requested` (more outputs) on its running sessions:
/// resizes the outputs it keeps where their size changes, records and
/// starts one more `RecordVirtual` stream per added output
/// (`ScreenCast.Stream.Start`, Mutter 41+), negotiates and names each, and
/// places them all. The kept outputs keep their streams and the
/// `RemoteDesktop` session stays. On failure every added stream is stopped
/// again (its monitor vanishes); the caller restores the kept sizes with
/// [`restore_outputs`].
///
/// # Errors
///
/// [`RemoteOutputError`].
pub async fn add_outputs(
    connection: &Connection,
    runtime_dir: &Path,
    set: &mut RemoteSet,
    requested: &[PlacedOutput],
) -> Result<(), RemoteOutputError> {
    let mut ops = LiveOps {
        connection,
        runtime_dir,
        set,
    };
    grow_with(&mut ops, requested, false).await
}

/// Shrinks `set` to `requested` (fewer outputs) on its running sessions:
/// stops the streams of the outputs it drops (their monitors vanish), then
/// resizes and places the ones it keeps. The kept outputs keep their
/// streams and the `RemoteDesktop` session stays. A dropped output leaves
/// the set even when the compositor refuses its stop. Rolling back is
/// [`restore_outputs`].
///
/// # Errors
///
/// [`RemoteOutputError`].
pub async fn remove_outputs(
    connection: &Connection,
    runtime_dir: &Path,
    set: &mut RemoteSet,
    requested: &[PlacedOutput],
) -> Result<(), RemoteOutputError> {
    let mut ops = LiveOps {
        connection,
        runtime_dir,
        set,
    };
    shrink_with(&mut ops, requested).await
}

/// Returns `set` to `previous` after a failed or rolled-back change of size
/// or count: see [`restore_with`].
///
/// # Errors
///
/// [`RemoteOutputError`] when the previous outputs cannot be restored.
pub async fn restore_outputs(
    connection: &Connection,
    runtime_dir: &Path,
    set: &mut RemoteSet,
    previous: &[PlacedOutput],
) -> Result<(), RemoteOutputError> {
    let mut ops = LiveOps {
        connection,
        runtime_dir,
        set,
    };
    restore_with(&mut ops, previous).await
}

/// Proves every output of `set` exists as a virtual monitor with its
/// requested geometry and that exactly they are placed as requested.
///
/// # Errors
///
/// [`RemoteOutputError::Geometry`] or [`RemoteOutputError::Placement`].
pub async fn verify(
    connection: &Connection,
    set: &RemoteSet,
) -> Result<Vec<VerifiedOutput>, RemoteOutputError> {
    let monitors = mutter::current_monitors(connection).await?;
    let observed = set
        .connectors
        .iter()
        .map(|connector| {
            monitors
                .iter()
                .find(|monitor| &monitor.connector == connector)
                .cloned()
                .unwrap_or(ObservedMonitor {
                    connector: connector.clone(),
                    is_virtual: false,
                    current_mode: None,
                })
        })
        .collect::<Vec<_>>();
    let requests = set
        .placed
        .iter()
        .map(|placed| placed.request)
        .collect::<Vec<_>>();
    let verified = verify_virtual_outputs(&requests, &observed)?;
    let state = mutter::display_state(connection).await?;
    let (targets, _scales) = layout_targets(&state, &set.placements())?;
    verify_placement(&targets, &state.placements)?;
    Ok(verified)
}

/// Keeps the desired placement: Mutter returns to its default layout after
/// any monitor change (a capture epoch negotiating another refresh, a
/// resize), so every `MonitorsChanged` is answered by placing the desired
/// outputs again. `None` pauses the keeper while a reconfiguration places
/// outputs itself.
pub async fn keep_placement(
    connection: Connection,
    mut desired: tokio::sync::watch::Receiver<DesiredPlacement>,
) {
    let Ok(mut changes) = mutter::monitors_changed(&connection).await else {
        tracing::warn!(target: target::SESSION, "MonitorsChanged could not be followed; output placement is not kept");
        return std::future::pending().await;
    };
    loop {
        tokio::select! {
            change = changes.next() => {
                if change.is_none() {
                    return std::future::pending().await;
                }
            }
            changed = desired.changed() => {
                if changed.is_err() {
                    return std::future::pending().await;
                }
            }
        }
        let Some(placements) = desired.borrow().clone() else {
            continue;
        };
        if let Err(error) = place(&connection, &placements).await {
            tracing::warn!(target: target::SESSION, %error, "virtual outputs could not be placed again");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_session::wayland_outputs::OutputDensity;

    /// A compositor whose monitors run what was last negotiated, refusing a
    /// placement whose sizes are not what its monitors run.
    #[derive(Default)]
    struct FakeCompositor {
        placed: Vec<PlacedOutput>,
        connectors: Vec<String>,
        /// The size each recorded stream's monitor runs.
        running: Vec<(u32, u32)>,
        next_connector: usize,
        renegotiations: usize,
        /// The renegotiation (counted from 1) that fails.
        fail_renegotiation: Option<usize>,
        /// The add (counted from 1) that fails.
        fail_add: Option<usize>,
        adds: usize,
        refuse_stops: bool,
    }

    impl FakeCompositor {
        fn live(outputs: &[PlacedOutput]) -> Self {
            let mut fake = Self {
                placed: outputs.to_vec(),
                ..Self::default()
            };
            for output in outputs {
                fake.running
                    .push((output.request.width(), output.request.height()));
                fake.connectors
                    .push(format!("Meta-{}", fake.next_connector));
                fake.next_connector += 1;
            }
            fake
        }

        fn consistent(&self) -> bool {
            self.placed.len() == self.connectors.len()
                && self.connectors.len() == self.running.len()
        }

        fn runs(&self, outputs: &[PlacedOutput]) -> bool {
            self.running
                == outputs
                    .iter()
                    .map(|output| (output.request.width(), output.request.height()))
                    .collect::<Vec<_>>()
        }
    }

    impl OutputOps for FakeCompositor {
        fn placed(&self) -> &[PlacedOutput] {
            &self.placed
        }

        fn placed_mut(&mut self) -> &mut Vec<PlacedOutput> {
            &mut self.placed
        }

        fn connectors(&self) -> &[String] {
            &self.connectors
        }

        fn connectors_mut(&mut self) -> &mut Vec<String> {
            &mut self.connectors
        }

        async fn renegotiate(
            &mut self,
            index: usize,
            request: VirtualOutputRequest,
        ) -> Result<(), RemoteOutputError> {
            self.renegotiations += 1;
            if self.fail_renegotiation == Some(self.renegotiations) {
                return Err(RemoteOutputError::Materialise {
                    index,
                    reason: "injected".to_owned(),
                });
            }
            self.running[index] = (request.width(), request.height());
            Ok(())
        }

        async fn add(
            &mut self,
            index: usize,
            request: VirtualOutputRequest,
        ) -> Result<String, RemoteOutputError> {
            self.adds += 1;
            if self.fail_add == Some(self.adds) {
                return Err(RemoteOutputError::MonitorMissing {
                    index,
                    width: request.width(),
                    height: request.height(),
                });
            }
            self.running.push((request.width(), request.height()));
            self.next_connector += 1;
            Ok(format!("Meta-{}", self.next_connector - 1))
        }

        async fn stop_last(&mut self) -> Result<(), RemoteOutputError> {
            self.running.pop();
            if self.refuse_stops {
                return Err(RemoteOutputError::Bus(MutterError::Reconfiguring));
            }
            Ok(())
        }

        async fn place(
            &mut self,
            placements: &[(String, PlacedOutput)],
        ) -> Result<(), RemoteOutputError> {
            let fits = placements.len() == self.running.len()
                && placements
                    .iter()
                    .zip(&self.running)
                    .all(|((_, output), running)| {
                        (output.request.width(), output.request.height()) == *running
                    });
            if fits {
                Ok(())
            } else {
                Err(RemoteOutputError::Placement(PlacementMismatch::Missing(
                    "overlap".to_owned(),
                )))
            }
        }

        fn assign(&mut self, _index: usize, _connector: &str, _placed: &PlacedOutput) {}
    }

    fn output(width: u32, height: u32, x: i32) -> PlacedOutput {
        PlacedOutput {
            request: VirtualOutputRequest::new(width, height, 60).unwrap(),
            x,
            y: 0,
            density: OutputDensity::default(),
        }
    }

    fn three() -> Vec<PlacedOutput> {
        vec![
            output(1920, 1080, 0),
            output(1920, 1080, 1920),
            output(1280, 720, 3840),
        ]
    }

    #[tokio::test]
    async fn a_shrink_failing_mid_resize_restores_every_kept_size() {
        let previous = three();
        let requested = vec![output(2560, 1440, 0), output(1600, 900, 2560)];
        let mut fake = FakeCompositor::live(&previous);
        // The first kept output is renegotiated, the second fails.
        fake.fail_renegotiation = Some(2);
        assert!(shrink_with(&mut fake, &requested).await.is_err());
        assert!(
            fake.consistent(),
            "bookkeeping matches the recorded streams"
        );
        assert_eq!(fake.running, vec![(2560, 1440), (1920, 1080)]);
        assert_eq!(
            fake.placed,
            previous[..2],
            "placed still records the old sizes"
        );

        // Growing back without renegotiating the kept outputs leaves the
        // compositor at the new size and the placement is refused.
        let mut unforced = FakeCompositor::live(&previous[..2]);
        unforced.running[0] = (2560, 1440);
        assert!(grow_with(&mut unforced, &previous, false).await.is_err());

        restore_with(&mut fake, &previous).await.unwrap();
        assert!(fake.consistent());
        assert!(fake.runs(&previous), "{:?}", fake.running);
        assert_eq!(fake.placed, previous);
    }

    #[tokio::test]
    async fn a_refused_stop_still_drops_the_output_from_every_list() {
        let previous = three();
        let mut fake = FakeCompositor::live(&previous);
        fake.refuse_stops = true;
        assert!(shrink_with(&mut fake, &previous[..2]).await.is_err());
        assert!(fake.consistent());
        assert_eq!(fake.connectors.len(), 2);

        fake.refuse_stops = false;
        restore_with(&mut fake, &previous).await.unwrap();
        assert!(fake.consistent());
        assert!(fake.runs(&previous));
        assert_eq!(fake.connectors, ["Meta-0", "Meta-1", "Meta-3"]);
    }

    #[tokio::test]
    async fn a_failed_grow_stops_what_it_added_and_restore_resizes_back() {
        let previous = vec![output(1920, 1080, 0)];
        let requested = vec![
            output(2560, 1440, 0),
            output(1920, 1080, 2560),
            output(1920, 1080, 4480),
        ];
        let mut fake = FakeCompositor::live(&previous);
        fake.fail_add = Some(2);
        assert!(grow_with(&mut fake, &requested, false).await.is_err());
        assert!(fake.consistent(), "the first added output is stopped again");
        assert_eq!(fake.running, vec![(2560, 1440)]);
        assert_eq!(fake.placed, previous);

        restore_with(&mut fake, &previous).await.unwrap();
        assert!(fake.consistent());
        assert!(fake.runs(&previous));
        assert_eq!(fake.placed, previous);
    }

    #[tokio::test]
    async fn restoring_while_stops_are_refused_reports_it_but_stays_consistent() {
        let previous = vec![output(1920, 1080, 0)];
        let mut fake = FakeCompositor::live(&three());
        fake.refuse_stops = true;
        assert!(restore_with(&mut fake, &previous).await.is_err());
        assert!(fake.consistent());
        assert!(fake.runs(&previous));
    }

    fn dense(width: u32, height: u32, x: i32, y: i32, percent: u32) -> PlacedOutput {
        let millimetres = |pixels: u32| {
            let value = f64::from(pixels) * 25.4 / (96.0 * f64::from(percent) / 100.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let rounded = value.round() as u32;
            rounded
        };
        PlacedOutput {
            request: VirtualOutputRequest::new(width, height, 60).unwrap(),
            x,
            y,
            density: OutputDensity {
                width_mm: millimetres(width),
                height_mm: millimetres(height),
                logical_width: width,
            },
        }
    }

    fn state(layout: LayoutMode, outputs: &[(String, PlacedOutput)]) -> mutter::DisplayState {
        let fractional = layout == LayoutMode::Logical;
        mutter::DisplayState {
            serial: 1,
            monitors: outputs
                .iter()
                .map(|(connector, placed)| mutter::DisplayMonitor {
                    connector: connector.clone(),
                    remote: true,
                    current: Some((
                        "mode".to_owned(),
                        placed.request.width(),
                        placed.request.height(),
                        60_000,
                    )),
                    supported_scales: if fractional {
                        vec![1.0, 1.25, 1.5, 1.75, 2.0]
                    } else {
                        vec![1.0, 1.5, 2.0]
                    },
                })
                .collect(),
            placements: Vec::new(),
            scales: Vec::new(),
            layout,
        }
    }

    fn named(outputs: Vec<PlacedOutput>) -> Vec<(String, PlacedOutput)> {
        outputs
            .into_iter()
            .enumerate()
            .map(|(index, placed)| (format!("Meta-{index}"), placed))
            .collect()
    }

    #[test]
    fn the_physical_layout_keeps_desktop_positions_and_whole_scales() {
        let outputs = named(vec![
            dense(1920, 1080, 0, 0, 150),
            dense(3840, 2160, 1920, 0, 200),
        ]);
        let (targets, scales) =
            layout_targets(&state(LayoutMode::Physical, &outputs), &outputs).unwrap();
        assert_eq!(targets, outputs);
        // 150% has no whole scale nearer than 1 (halfway takes the smaller);
        // the physical mode never takes the offered 1.5.
        assert_eq!(scales, vec![1.0, 2.0]);
    }

    #[test]
    fn the_logical_layout_moves_outputs_by_their_scales() {
        // A HiDPI output left of a 1080p one.
        let outputs = named(vec![
            dense(1920, 1080, 3840, 0, 100),
            dense(3840, 2160, 0, 0, 200),
        ]);
        let (targets, scales) =
            layout_targets(&state(LayoutMode::Logical, &outputs), &outputs).unwrap();
        assert_eq!(scales, vec![1.0, 2.0]);
        assert_eq!(
            targets
                .iter()
                .map(|(_, placed)| (placed.x, placed.y))
                .collect::<Vec<_>>(),
            vec![(1920, 0), (0, 0)]
        );
        // Fractional scales are taken where the logical mode offers them.
        let outputs = named(vec![
            dense(2560, 1440, 0, 0, 150),
            dense(1920, 1080, 2560, 0, 100),
        ]);
        let (targets, scales) =
            layout_targets(&state(LayoutMode::Logical, &outputs), &outputs).unwrap();
        assert_eq!(scales, vec![1.5, 1.0]);
        assert_eq!((targets[1].1.x, targets[1].1.y), (1707, 0));
    }

    #[test]
    fn scales_without_a_logical_layout_fall_back_to_unscaled() {
        let outputs = named(vec![
            dense(1920, 1080, 0, 0, 200),
            dense(1920, 1080, 1920, 0, 100),
            dense(1920, 1080, 0, 1080, 100),
            dense(1920, 1080, 1920, 1080, 100),
        ]);
        let (targets, scales) =
            layout_targets(&state(LayoutMode::Logical, &outputs), &outputs).unwrap();
        assert_eq!(scales, vec![1.0; 4]);
        assert_eq!(targets, outputs);
    }
}
