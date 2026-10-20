//! Mutter's D-Bus interfaces on a headless session's private bus:
//! `DisplayConfig` (monitor facts), `ScreenCast` (streams) and
//! `RemoteDesktop` (input), through the pure-Rust `zbus` client.
//!
//! The `RemoteDesktop` session is created first and the `ScreenCast` session
//! is linked to it (`remote-desktop-session-id`), so absolute pointer and
//! touch coordinates address a stream. Starting the `RemoteDesktop` session
//! starts both. Mutter closes both when this connection goes away, so the
//! owner keeps [`MutterSessions`] — and its connection — alive for exactly
//! as long as the stream may be consumed.

use std::collections::HashMap;
use std::time::Duration;

use std::sync::{Arc, Mutex, PoisonError};

use arcen_session::wayland_outputs::{absolute_stream_coordinate, AbsoluteInputStream, LayoutMode};
use arcen_session::wayland_session::{ObservedMonitor, RemoteDesktopEvidence, ScreenCastStream};
use futures_util::StreamExt;
use thiserror::Error;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

use super::input::CompositorInput;
use super::{InputCommand, ReadyStream, INITIAL_OUTPUT_GENERATION};
use crate::logging::target;

const SCREEN_CAST: &str = "org.gnome.Mutter.ScreenCast";
const SCREEN_CAST_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SCREEN_CAST_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const SCREEN_CAST_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
const REMOTE_DESKTOP: &str = "org.gnome.Mutter.RemoteDesktop";
const REMOTE_DESKTOP_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const REMOTE_DESKTOP_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_CONFIG_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const VIRTUAL_MONITOR_PRODUCT: &str = "MetaVirtualMonitor";
/// Product name of a `ScreenCast.RecordVirtual` monitor.
const REMOTE_MONITOR_PRODUCT: &str = "Virtual remote monitor";

const NAME_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// A Mutter D-Bus failure.
#[derive(Debug, Error)]
pub enum MutterError {
    #[error("the session's outputs are being reconfigured; input refused")]
    Reconfiguring,
    #[error("D-Bus call failed: {0}")]
    Bus(#[from] zbus::Error),
    #[error("{0} did not appear on the session bus in time")]
    NameTimeout(&'static str),
    #[error("ScreenCast stream for {0} announced no PipeWire node in time")]
    StreamTimeout(String),
    #[error("ScreenCast stream parameters are malformed")]
    Parameters,
    #[error("D-Bus value is malformed: {0}")]
    Value(#[from] zbus::zvariant::Error),
    #[error("{0} is missing from the compositor's interface")]
    Missing(&'static str),
    #[error("{0} was not answered in time")]
    CallTimeout(&'static str),
    #[error("the session has no stylus device: {0}")]
    NoStylus(arcen_input::pen_emulation::StylusUnavailable),
}

type ModeTuple = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type MonitorTuple = (
    (String, String, String, String),
    Vec<ModeTuple>,
    HashMap<String, OwnedValue>,
);
type LogicalMonitorTuple = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, OwnedValue>,
);
type CurrentState = (
    u32,
    Vec<MonitorTuple>,
    Vec<LogicalMonitorTuple>,
    HashMap<String, OwnedValue>,
);

/// Connects to the private bus at `address`.
///
/// # Errors
///
/// [`MutterError::Bus`] when the bus cannot be reached.
pub async fn connect(address: &str) -> Result<Connection, MutterError> {
    Ok(zbus::connection::Builder::address(address)?.build().await?)
}

/// Waits until `name` has an owner on the bus.
///
/// # Errors
///
/// [`MutterError::NameTimeout`] when it does not appear in time.
pub async fn wait_for_name(connection: &Connection, name: &'static str) -> Result<(), MutterError> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let bus_name = zbus::names::BusName::try_from(name).map_err(zbus::Error::from)?;
    let deadline = tokio::time::Instant::now() + NAME_TIMEOUT;
    loop {
        if bus.name_has_owner(bus_name.clone()).await.unwrap_or(false) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(MutterError::NameTimeout(name));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The compositor's monitors, as `DisplayConfig.GetCurrentState` reports
/// them, in report order.
///
/// # Errors
///
/// [`MutterError`] when `DisplayConfig` is missing or malformed.
pub async fn current_monitors(
    connection: &Connection,
) -> Result<Vec<ObservedMonitor>, MutterError> {
    wait_for_name(connection, DISPLAY_CONFIG).await?;
    let proxy = Proxy::new(
        connection,
        DISPLAY_CONFIG,
        DISPLAY_CONFIG_PATH,
        DISPLAY_CONFIG,
    )
    .await?;
    let (_serial, monitors, _logical, _properties): CurrentState =
        proxy.call("GetCurrentState", &()).await?;
    Ok(monitors
        .into_iter()
        .map(
            |((connector, _vendor, product, _serial), modes, _properties)| {
                let current_mode = modes
                    .iter()
                    .find(|mode| {
                        mode.6
                            .get("is-current")
                            .and_then(|value| value.downcast_ref::<bool>().ok())
                            .unwrap_or(false)
                    })
                    .and_then(|(_, width, height, refresh, ..)| {
                        Some((
                            u32::try_from(*width).ok()?,
                            u32::try_from(*height).ok()?,
                            millihertz(*refresh)?,
                        ))
                    });
                ObservedMonitor {
                    connector,
                    is_virtual: product == VIRTUAL_MONITOR_PRODUCT
                        || product == REMOTE_MONITOR_PRODUCT,
                    current_mode,
                }
            },
        )
        .collect())
}

/// One monitor with the mode it runs, for placing it.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayMonitor {
    /// Connector.
    pub connector: String,
    /// Whether it is a `RecordVirtual` monitor.
    pub remote: bool,
    /// Current mode id, width, height and refresh in millihertz.
    pub current: Option<(String, u32, u32, u32)>,
    /// The UI scales the current mode supports.
    pub supported_scales: Vec<f64>,
}

/// `DisplayConfig.GetCurrentState`, reduced to what placement needs.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayState {
    /// Configuration serial, required by `ApplyMonitorsConfig`.
    pub serial: u32,
    /// Monitors.
    pub monitors: Vec<DisplayMonitor>,
    /// Enabled logical monitors with their first monitor's connector.
    pub placements: Vec<arcen_session::wayland_outputs::ObservedPlacement>,
    /// Each enabled logical monitor's first connector and its UI scale.
    pub scales: Vec<(String, f64)>,
    /// How positions are expressed (`layout-mode`).
    pub layout: arcen_session::wayland_outputs::LayoutMode,
}

/// The compositor's monitors, modes and logical layout.
///
/// # Errors
///
/// [`MutterError`] when `DisplayConfig` is missing or malformed.
pub async fn display_state(connection: &Connection) -> Result<DisplayState, MutterError> {
    let proxy = Proxy::new(
        connection,
        DISPLAY_CONFIG,
        DISPLAY_CONFIG_PATH,
        DISPLAY_CONFIG,
    )
    .await?;
    let (serial, monitors, logical, properties): CurrentState =
        proxy.call("GetCurrentState", &()).await?;
    let layout = arcen_session::wayland_outputs::LayoutMode::from_mutter(
        properties
            .get("layout-mode")
            .and_then(|value| value.downcast_ref::<u32>().ok()),
    );
    let monitors = monitors
        .into_iter()
        .map(
            |((connector, _vendor, product, _serial), modes, _properties)| {
                let current_mode = modes.iter().find(|mode| {
                    mode.6
                        .get("is-current")
                        .and_then(|value| value.downcast_ref::<bool>().ok())
                        .unwrap_or(false)
                });
                let current = current_mode.and_then(|(id, width, height, refresh, ..)| {
                    Some((
                        id.clone(),
                        u32::try_from(*width).ok()?,
                        u32::try_from(*height).ok()?,
                        millihertz(*refresh)?,
                    ))
                });
                DisplayMonitor {
                    connector,
                    remote: product == REMOTE_MONITOR_PRODUCT,
                    current,
                    supported_scales: current_mode.map(|mode| mode.5.clone()).unwrap_or_default(),
                }
            },
        )
        .collect();
    let scales = logical
        .iter()
        .filter_map(|(_, _, scale, _, _, monitors, _)| {
            monitors.first().map(|monitor| (monitor.0.clone(), *scale))
        })
        .collect();
    let placements = logical
        .into_iter()
        .filter_map(
            |(x, y, _scale, _transform, _primary, monitors, _properties)| {
                monitors.first().map(
                    |monitor| arcen_session::wayland_outputs::ObservedPlacement {
                        connector: monitor.0.clone(),
                        x,
                        y,
                    },
                )
            },
        )
        .collect();
    Ok(DisplayState {
        serial,
        monitors,
        placements,
        scales,
        layout,
    })
}

/// One `ApplyMonitorsConfig` logical monitor: x, y, scale, transform,
/// primary, and its monitors (connector, mode id, properties).
type LogicalMonitorConfig<'a> = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(&'a str, &'a str, HashMap<&'a str, Value<'a>>)>,
);

/// Places exactly `placements` (connector, mode id, x, y, UI scale; the
/// first is primary) with `ApplyMonitorsConfig` (temporary method, no
/// transform); every other monitor is disabled. Positions are in the
/// compositor's layout mode ([`DisplayState::layout`]): physical pixels, or
/// logical pixels under `scale-monitor-framebuffer`.
///
/// # Errors
///
/// [`MutterError`] when Mutter refuses the configuration.
pub async fn apply_layout(
    connection: &Connection,
    serial: u32,
    placements: &[(String, String, i32, i32, f64)],
) -> Result<(), MutterError> {
    const METHOD_TEMPORARY: u32 = 1;
    let proxy = Proxy::new(
        connection,
        DISPLAY_CONFIG,
        DISPLAY_CONFIG_PATH,
        DISPLAY_CONFIG,
    )
    .await?;
    let logical: Vec<LogicalMonitorConfig<'_>> = placements
        .iter()
        .enumerate()
        .map(|(index, (connector, mode, x, y, scale))| {
            (
                *x,
                *y,
                *scale,
                0,
                index == 0,
                vec![(connector.as_str(), mode.as_str(), HashMap::new())],
            )
        })
        .collect();
    let properties: HashMap<&str, Value<'_>> = HashMap::new();
    proxy
        .call::<_, _, ()>(
            "ApplyMonitorsConfig",
            &(serial, METHOD_TEMPORARY, logical, properties),
        )
        .await?;
    Ok(())
}

/// Resolves on each `DisplayConfig.MonitorsChanged`.
///
/// # Errors
///
/// [`MutterError`] when the signal cannot be subscribed.
pub async fn monitors_changed(
    connection: &Connection,
) -> Result<impl futures_util::Stream<Item = zbus::Message> + Unpin, MutterError> {
    let proxy = Proxy::new(
        connection,
        DISPLAY_CONFIG,
        DISPLAY_CONFIG_PATH,
        DISPLAY_CONFIG,
    )
    .await?;
    Ok(proxy.receive_signal("MonitorsChanged").await?)
}

fn millihertz(refresh: f64) -> Option<u32> {
    let millihertz = (refresh * 1000.0).round();
    (millihertz.is_finite() && (0.0..=f64::from(u32::MAX)).contains(&millihertz))
        // In range and rounded, so the cast is exact.
        .then_some(millihertz as u32)
}

/// What one stream records.
#[derive(Debug, Clone)]
enum RecordTarget {
    /// An existing monitor (`RecordMonitor`).
    Monitor(String),
    /// A new virtual monitor (`RecordVirtual`), by record index.
    Virtual(usize),
}

/// The connector a virtual stream carries until its monitor exists.
#[must_use]
pub fn virtual_placeholder(index: usize) -> String {
    format!("virtual-{index}")
}

/// One recorded output inside the paired sessions.
struct RecordedStream {
    stream: ScreenCastStream,
    position: (i32, i32),
    path: OwnedObjectPath,
    kind: AbsoluteInputStream,
}

/// The paired `RemoteDesktop` + `ScreenCast` sessions of one headless
/// desktop. Dropping it without [`MutterSessions::stop`] still ends both
/// sessions when the connection closes.
pub struct MutterSessions {
    connection: Connection,
    remote_desktop: Proxy<'static>,
    screen_cast_session: Proxy<'static>,
    streams: Vec<RecordedStream>,
    /// Unstarted `RecordMonitor` streams of this session's own outputs that
    /// absolute input addresses while another stream is consumed (see
    /// [`Self::record_input_monitors`]).
    input_monitors: Mutex<InputMonitors>,
    /// The compositor's layout mode and monitor scales, followed from
    /// `MonitorsChanged`, for converting absolute coordinates.
    input_geometry: Arc<Mutex<InputGeometry>>,
    geometry_follower: Option<tokio::task::AbortHandle>,
    device_types: u32,
    stopped: bool,
}

impl Drop for MutterSessions {
    fn drop(&mut self) {
        if let Some(follower) = self.geometry_follower.take() {
            follower.abort();
        }
    }
}

/// What absolute input needs of the compositor's layout: how positions are
/// expressed and each enabled monitor's UI scale.
#[derive(Debug, Clone, PartialEq)]
struct InputGeometry {
    layout: LayoutMode,
    scales: Vec<(String, f64)>,
}

impl Default for InputGeometry {
    fn default() -> Self {
        Self {
            layout: LayoutMode::Physical,
            scales: Vec::new(),
        }
    }
}

impl InputGeometry {
    fn from_state(state: &DisplayState) -> Self {
        Self {
            layout: state.layout,
            scales: state.scales.clone(),
        }
    }

    /// The UI scale of `connector`'s monitor; 1 when it is not enabled.
    fn scale(&self, connector: &str) -> f64 {
        self.scales
            .iter()
            .find(|(name, _)| name == connector)
            .map_or(1.0, |(_, scale)| *scale)
    }

    /// The coordinate Mutter expects on a stream of `kind` for `point` in
    /// `connector`'s output pixels.
    fn stream_coordinate(
        &self,
        kind: AbsoluteInputStream,
        connector: &str,
        point: (f64, f64),
    ) -> (f64, f64) {
        absolute_stream_coordinate(kind, self.layout, self.scale(connector), point)
    }
}

/// Reads the layout mode and scales into `geometry`; keeps the previous
/// values when `DisplayConfig` cannot be read.
async fn refresh_geometry(connection: &Connection, geometry: &Mutex<InputGeometry>) {
    match display_state(connection).await {
        Ok(state) => {
            let fresh = InputGeometry::from_state(&state);
            let mut current = geometry.lock().unwrap_or_else(PoisonError::into_inner);
            if *current != fresh {
                tracing::info!(
                    target: target::INPUT,
                    layout = ?fresh.layout,
                    scales = ?fresh.scales,
                    "absolute input follows the compositor's layout"
                );
                *current = fresh;
            }
        }
        Err(error) => {
            tracing::warn!(
                target: target::INPUT,
                %error,
                "the compositor's layout could not be read; absolute input keeps the previous scales"
            );
        }
    }
}

/// The calls that take an absolute coordinate on a stream.
#[derive(Debug, Clone, Copy)]
enum AbsoluteCall {
    Pointer,
    TouchDown(u32),
    TouchMotion(u32),
}

/// The stream an absolute coordinate addresses and how Mutter maps it.
struct AbsoluteTarget {
    path: OwnedObjectPath,
    kind: AbsoluteInputStream,
    /// An input monitor stream (see [`MutterSessions::record_input_monitors`]).
    input_monitor: bool,
}

/// Whether `error` is `RemoteDesktop` naming a stream it no longer has:
/// Mutter closes a monitor stream when its monitor's logical rectangle
/// changes or the monitor goes away.
fn unknown_stream(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(_, Some(message), _) if message == "Unknown stream")
}

/// Input monitor streams: every one recorded (Mutter keeps an unstarted
/// stream until the session ends, so they are reused across epochs) and the
/// ones absolute input addresses now.
#[derive(Default)]
struct InputMonitors {
    recorded: Vec<(String, OwnedObjectPath)>,
    active: Vec<(String, OwnedObjectPath)>,
    /// Whether absolute input addresses input monitors (set by
    /// [`MutterSessions::record_input_monitors`], cleared on release),
    /// independent of how many are active.
    engaged: bool,
    /// Bumped by every engage and release, so a replacement recorded across
    /// one is not adopted.
    epoch: u64,
}

impl InputMonitors {
    fn engage(&mut self, active: Vec<(String, OwnedObjectPath)>) {
        for (connector, path) in &active {
            self.recorded.retain(|(name, _)| name != connector);
            self.recorded.push((connector.clone(), path.clone()));
        }
        self.active = active;
        self.engaged = true;
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn release(&mut self) {
        self.active.clear();
        self.engaged = false;
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Forgets the closed stream `dead`. Returns the epoch to adopt a
    /// replacement in when input was addressing it.
    fn forget(&mut self, dead: &OwnedObjectPath) -> Option<u64> {
        self.recorded.retain(|(_, path)| path != dead);
        let before = self.active.len();
        self.active.retain(|(_, path)| path != dead);
        (self.engaged && self.active.len() != before).then_some(self.epoch)
    }

    /// Takes `path`, recorded for `connector` to replace a stream forgotten
    /// in `epoch`: absolute input addresses it unless input was released or
    /// engaged again meanwhile. Returns whether it did.
    fn adopt(&mut self, connector: &str, path: OwnedObjectPath, epoch: u64) -> bool {
        if epoch != self.epoch || !self.engaged {
            if !self.recorded.iter().any(|(name, _)| name == connector) {
                self.recorded.push((connector.to_owned(), path));
            }
            return false;
        }
        self.recorded.retain(|(name, _)| name != connector);
        self.recorded.push((connector.to_owned(), path.clone()));
        self.active.retain(|(name, _)| name != connector);
        self.active.push((connector.to_owned(), path));
        true
    }
}

/// The stream an absolute coordinate on `connector` addresses: an input
/// monitor stream when one is recorded (`true`), else the output's own
/// stream, with the kind Mutter maps it as.
fn absolute_target<'a, T>(
    input_monitors: &'a [(String, T)],
    mut recorded: impl Iterator<Item = (&'a str, &'a T, AbsoluteInputStream)>,
    connector: &str,
) -> Option<(&'a T, AbsoluteInputStream, bool)> {
    input_monitors
        .iter()
        .find(|(monitor, _)| monitor == connector)
        .map(|(_, target)| (target, AbsoluteInputStream::Monitor, true))
        .or_else(|| {
            recorded
                .find(|(name, _, _)| *name == connector)
                .map(|(_, target, kind)| (target, kind, false))
        })
}

impl MutterSessions {
    /// Creates a `RemoteDesktop` session, links a `ScreenCast` session to it,
    /// records each connector with `cursor_mode`, starts them, and waits for
    /// every stream's `PipeWire` node.
    ///
    /// # Errors
    ///
    /// [`MutterError`] when any step fails or a stream announces no node.
    pub async fn start(
        connection: &Connection,
        connectors: &[String],
        cursor_mode: u32,
    ) -> Result<Self, MutterError> {
        let targets = connectors
            .iter()
            .map(|connector| RecordTarget::Monitor(connector.clone()))
            .collect::<Vec<_>>();
        Self::start_targets(connection, &targets, cursor_mode).await
    }

    /// Like [`Self::start`], but records `count` new `RecordVirtual`
    /// streams (`is-platform`, so GNOME treats them as real monitors).
    /// Mutter creates each stream's monitor only when a consumer negotiates
    /// its size; until [`Self::assign`] names it, a stream has a placeholder
    /// connector `virtual-<index>` and size zero.
    ///
    /// # Errors
    ///
    /// As [`Self::start`].
    pub async fn start_virtual(
        connection: &Connection,
        count: usize,
        cursor_mode: u32,
    ) -> Result<Self, MutterError> {
        let targets = (0..count).map(RecordTarget::Virtual).collect::<Vec<_>>();
        Self::start_targets(connection, &targets, cursor_mode).await
    }

    async fn start_targets(
        connection: &Connection,
        targets: &[RecordTarget],
        cursor_mode: u32,
    ) -> Result<Self, MutterError> {
        wait_for_name(connection, REMOTE_DESKTOP).await?;
        wait_for_name(connection, SCREEN_CAST).await?;
        let remote_desktop_root = Proxy::new(
            connection,
            REMOTE_DESKTOP,
            REMOTE_DESKTOP_PATH,
            REMOTE_DESKTOP,
        )
        .await?;
        let device_types: u32 = remote_desktop_root
            .get_property("SupportedDeviceTypes")
            .await?;
        let remote_desktop_path: OwnedObjectPath =
            remote_desktop_root.call("CreateSession", &()).await?;
        let remote_desktop = Proxy::new(
            connection,
            REMOTE_DESKTOP,
            remote_desktop_path,
            REMOTE_DESKTOP_SESSION,
        )
        .await?;
        let remote_desktop_id: String = remote_desktop.get_property("SessionId").await?;

        let screen_cast_root =
            Proxy::new(connection, SCREEN_CAST, SCREEN_CAST_PATH, SCREEN_CAST).await?;
        let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
        properties.insert(
            "remote-desktop-session-id",
            Value::from(remote_desktop_id.as_str()),
        );
        let screen_cast_path: OwnedObjectPath = screen_cast_root
            .call("CreateSession", &(properties,))
            .await?;
        let screen_cast_session = Proxy::new(
            connection,
            SCREEN_CAST,
            screen_cast_path,
            SCREEN_CAST_SESSION,
        )
        .await?;

        let mut pending = Vec::new();
        for target in targets {
            let mut options: HashMap<&str, Value<'_>> = HashMap::new();
            options.insert("cursor-mode", Value::from(cursor_mode));
            let (connector, stream_path): (String, OwnedObjectPath) = match target {
                RecordTarget::Monitor(connector) => (
                    connector.clone(),
                    screen_cast_session
                        .call("RecordMonitor", &(connector.as_str(), options))
                        .await?,
                ),
                RecordTarget::Virtual(index) => {
                    options.insert("is-platform", Value::from(true));
                    (
                        virtual_placeholder(*index),
                        screen_cast_session
                            .call("RecordVirtual", &(options,))
                            .await?,
                    )
                }
            };
            let stream_proxy = Proxy::new(
                connection,
                SCREEN_CAST,
                stream_path.clone(),
                SCREEN_CAST_STREAM,
            )
            .await?;
            let added = stream_proxy.receive_signal("PipeWireStreamAdded").await?;
            pending.push((connector, stream_path, stream_proxy, added, target));
        }

        let mut sessions = Self {
            connection: connection.clone(),
            remote_desktop,
            screen_cast_session,
            streams: Vec::new(),
            input_monitors: Mutex::new(InputMonitors::default()),
            input_geometry: Arc::new(Mutex::new(InputGeometry::default())),
            geometry_follower: None,
            device_types,
            stopped: false,
        };
        sessions.follow_geometry().await;
        sessions
            .remote_desktop
            .call::<_, _, ()>("Start", &())
            .await?;

        for (connector, path, proxy, mut added, target) in pending {
            let message = tokio::time::timeout(STREAM_TIMEOUT, added.next())
                .await
                .ok()
                .flatten()
                .ok_or_else(|| MutterError::StreamTimeout(connector.clone()))?;
            let (node_id,): (u32,) = message.body().deserialize()?;
            let kind = match target {
                RecordTarget::Monitor(_) => AbsoluteInputStream::Monitor,
                RecordTarget::Virtual(_) => AbsoluteInputStream::Virtual,
            };
            let (width, height, position) = match target {
                RecordTarget::Monitor(_) => {
                    let parameters: HashMap<String, OwnedValue> =
                        proxy.get_property("Parameters").await?;
                    let (width, height) = pair(&parameters, "size")?;
                    let position = pair(&parameters, "position").unwrap_or((0, 0));
                    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height))
                    else {
                        return Err(MutterError::Parameters);
                    };
                    (width, height, position)
                }
                // A virtual stream has no monitor, size or position until a
                // consumer negotiates one.
                RecordTarget::Virtual(_) => (0, 0, (0, 0)),
            };
            tracing::info!(
                target: target::SESSION,
                connector = %connector,
                node_id,
                width,
                height,
                x = position.0,
                y = position.1,
                cursor_mode,
                "Mutter ScreenCast stream announced its PipeWire node"
            );
            sessions.streams.push(RecordedStream {
                stream: ScreenCastStream {
                    connector,
                    node_id,
                    width,
                    height,
                },
                position,
                path,
                kind,
            });
        }
        Ok(sessions)
    }

    /// Follows `MonitorsChanged` for the layout mode and monitor scales
    /// absolute input is converted with, until the sessions are dropped.
    async fn follow_geometry(&mut self) {
        let mut changes = match monitors_changed(&self.connection).await {
            Ok(changes) => changes,
            Err(error) => {
                tracing::warn!(
                    target: target::INPUT,
                    %error,
                    "MonitorsChanged could not be followed; absolute input assumes unscaled outputs"
                );
                return;
            }
        };
        refresh_geometry(&self.connection, &self.input_geometry).await;
        let connection = self.connection.clone();
        let geometry = Arc::clone(&self.input_geometry);
        let follower = tokio::spawn(async move {
            while changes.next().await.is_some() {
                refresh_geometry(&connection, &geometry).await;
            }
        });
        self.geometry_follower = Some(follower.abort_handle());
    }

    /// Reads the layout mode and monitor scales again now, rather than on
    /// the next `MonitorsChanged`: call after placing outputs, before input
    /// may address them.
    pub async fn refresh_input_geometry(&self) {
        refresh_geometry(&self.connection, &self.input_geometry).await;
    }

    /// `PipeWire` node of each stream, in record order.
    #[must_use]
    pub fn node_ids(&self) -> Vec<u32> {
        self.streams
            .iter()
            .map(|recorded| recorded.stream.node_id)
            .collect()
    }

    /// Records one more `RecordVirtual` stream on these running sessions
    /// and starts it with `ScreenCast.Stream.Start` (a stream recorded
    /// after the session started announces its node only then; Mutter 41
    /// and later). Its monitor appears when a consumer negotiates its size;
    /// until [`Self::assign`] it carries the placeholder connector of its
    /// index. Returns the new stream's node.
    ///
    /// # Errors
    ///
    /// [`MutterError`] when Mutter refuses the stream or announces no node.
    pub async fn add_virtual(&mut self, cursor_mode: u32) -> Result<u32, MutterError> {
        let index = self.streams.len();
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("cursor-mode", Value::from(cursor_mode));
        options.insert("is-platform", Value::from(true));
        let path: OwnedObjectPath = self
            .screen_cast_session
            .call("RecordVirtual", &(options,))
            .await?;
        let proxy = Proxy::new(
            &self.connection,
            SCREEN_CAST,
            path.clone(),
            SCREEN_CAST_STREAM,
        )
        .await?;
        let mut added = proxy.receive_signal("PipeWireStreamAdded").await?;
        let connector = virtual_placeholder(index);
        let node = async {
            proxy.call::<_, _, ()>("Start", &()).await?;
            let message = tokio::time::timeout(STREAM_TIMEOUT, added.next())
                .await
                .ok()
                .flatten()
                .ok_or_else(|| MutterError::StreamTimeout(connector.clone()))?;
            let (node_id,): (u32,) = message.body().deserialize()?;
            Ok::<u32, MutterError>(node_id)
        }
        .await;
        let node_id = match node {
            Ok(node_id) => node_id,
            Err(error) => {
                let _ = proxy.call::<_, _, ()>("Stop", &()).await;
                return Err(error);
            }
        };
        tracing::info!(
            target: target::SESSION,
            connector = %connector,
            node_id,
            cursor_mode,
            "Mutter ScreenCast stream added to the running session announced its PipeWire node"
        );
        self.streams.push(RecordedStream {
            stream: ScreenCastStream {
                connector,
                node_id,
                width: 0,
                height: 0,
            },
            position: (0, 0),
            path,
            kind: AbsoluteInputStream::Virtual,
        });
        Ok(node_id)
    }

    /// Stops the last stream (`ScreenCast.Stream.Stop`), which removes its
    /// virtual monitor, and forgets it. No consumer may read it.
    ///
    /// # Errors
    ///
    /// [`MutterError`] when Mutter refuses; the stream is forgotten anyway
    /// (it cannot be used again).
    pub async fn stop_last_stream(&mut self) -> Result<(), MutterError> {
        let Some(recorded) = self.streams.pop() else {
            return Ok(());
        };
        let proxy = Proxy::new(
            &self.connection,
            SCREEN_CAST,
            recorded.path,
            SCREEN_CAST_STREAM,
        )
        .await?;
        proxy.call::<_, _, ()>("Stop", &()).await?;
        tracing::info!(
            target: target::SESSION,
            connector = %recorded.stream.connector,
            node_id = recorded.stream.node_id,
            "Mutter ScreenCast stream stopped; its virtual monitor is removed"
        );
        Ok(())
    }

    /// Names stream `index` after the monitor its consumer created, with
    /// that monitor's size and place.
    pub fn assign(
        &mut self,
        index: usize,
        connector: &str,
        size: (u32, u32),
        position: (i32, i32),
    ) {
        if let Some(recorded) = self.streams.get_mut(index) {
            recorded.stream.connector = connector.to_owned();
            recorded.stream.width = size.0;
            recorded.stream.height = size.1;
            recorded.position = position;
        }
    }

    /// The announced streams, in connector order.
    #[must_use]
    pub fn streams(&self) -> Vec<ScreenCastStream> {
        self.streams
            .iter()
            .map(|recorded| recorded.stream.clone())
            .collect()
    }

    /// The streams in the agent's ready shape.
    #[must_use]
    pub fn ready_streams(&self) -> Vec<ReadyStream> {
        self.streams
            .iter()
            .map(|recorded| ReadyStream {
                connector: recorded.stream.connector.clone(),
                node_id: recorded.stream.node_id,
                width: recorded.stream.width,
                height: recorded.stream.height,
                x: recorded.position.0,
                y: recorded.position.1,
            })
            .collect()
    }

    /// `SupportedDeviceTypes` of the `RemoteDesktop` service.
    #[must_use]
    pub const fn device_types(&self) -> u32 {
        self.device_types
    }

    /// Proves the input target: an absolute pointer motion to the centre of
    /// the first stream is accepted.
    ///
    /// # Errors
    ///
    /// [`MutterError`] when `RemoteDesktop` refuses it.
    pub async fn probe_input(&self) -> Result<RemoteDesktopEvidence, MutterError> {
        let first = self.streams.first().ok_or(MutterError::Parameters)?;
        self.notify(&InputCommand::PointerAbsolute {
            output: first.stream.connector.clone(),
            generation: INITIAL_OUTPUT_GENERATION,
            x: f64::from(first.stream.width) / 2.0,
            y: f64::from(first.stream.height) / 2.0,
        })
        .await?;
        Ok(RemoteDesktopEvidence {
            device_types: self.device_types,
            started: true,
            linked_to_screencast: true,
            absolute_pointer_accepted: true,
        })
    }

    fn absolute_target(&self, connector: &str) -> Result<AbsoluteTarget, MutterError> {
        let input_monitors = self
            .input_monitors
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        absolute_target(
            &input_monitors.active,
            self.streams.iter().map(|recorded| {
                (
                    recorded.stream.connector.as_str(),
                    &recorded.path,
                    recorded.kind,
                )
            }),
            connector,
        )
        .map(|(path, kind, input_monitor)| AbsoluteTarget {
            path: path.clone(),
            kind,
            input_monitor,
        })
        .ok_or(MutterError::Parameters)
    }

    /// Injects an absolute coordinate given in `connector`'s output pixels,
    /// converted to what Mutter expects of the stream it addresses. An input
    /// monitor stream Mutter has closed (its monitor moved, resized or
    /// rescaled within the output generation) is recorded again for the
    /// monitor as it is now, or dropped for the output's own stream, and the
    /// command is given once more.
    async fn notify_absolute(
        &self,
        call: AbsoluteCall,
        connector: &str,
        point: (f64, f64),
    ) -> Result<(), MutterError> {
        let target = self.absolute_target(connector)?;
        match self.call_absolute(call, &target, connector, point).await {
            Err(MutterError::Bus(error)) if target.input_monitor && unknown_stream(&error) => {
                self.replace_input_monitor(connector, &target.path).await;
                let target = self.absolute_target(connector)?;
                self.call_absolute(call, &target, connector, point).await
            }
            result => result,
        }
    }

    async fn call_absolute(
        &self,
        call: AbsoluteCall,
        target: &AbsoluteTarget,
        connector: &str,
        point: (f64, f64),
    ) -> Result<(), MutterError> {
        let (x, y) = self
            .input_geometry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stream_coordinate(target.kind, connector, point);
        let rd = &self.remote_desktop;
        let path = target.path.as_str();
        match call {
            AbsoluteCall::Pointer => {
                rd.call::<_, _, ()>("NotifyPointerMotionAbsolute", &(path, x, y))
                    .await?;
            }
            AbsoluteCall::TouchDown(slot) => {
                rd.call::<_, _, ()>("NotifyTouchDown", &(path, slot, x, y))
                    .await?;
            }
            AbsoluteCall::TouchMotion(slot) => {
                rd.call::<_, _, ()>("NotifyTouchMotion", &(path, slot, x, y))
                    .await?;
            }
        }
        Ok(())
    }

    /// Forgets the closed input monitor stream `dead` of `connector` and,
    /// while input still addresses input monitors, records the monitor
    /// again; without one, `connector`'s input returns to its own stream.
    async fn replace_input_monitor(&self, connector: &str, dead: &OwnedObjectPath) {
        let Some(epoch) = self
            .input_monitors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .forget(dead)
        else {
            return;
        };
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("cursor-mode", Value::from(0_u32));
        let recorded: Result<OwnedObjectPath, zbus::Error> = self
            .screen_cast_session
            .call("RecordMonitor", &(connector, options))
            .await;
        match recorded {
            Ok(path) => {
                let adopted = self
                    .input_monitors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .adopt(connector, path, epoch);
                tracing::info!(
                    target: target::INPUT,
                    connector,
                    adopted,
                    "an input monitor stream was closed by the compositor; recorded it again"
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: target::INPUT,
                    connector,
                    %error,
                    "an input monitor stream was closed and could not be recorded again; absolute input returns to the output's own stream"
                );
            }
        }
    }

    /// Records each of `connectors` again with `RecordMonitor` in this
    /// (`RemoteDesktop`-linked) session, never started, and addresses
    /// absolute pointer and touch input to those streams until
    /// [`Self::release_input_monitors`].
    ///
    /// Mutter applies an absolute coordinate given for a `RecordVirtual`
    /// stream only while that stream is consumed (`transform_position`
    /// needs its source enabled); a monitor stream maps through the logical
    /// monitor's layout whether or not anything consumes it. A wide-capture
    /// epoch consumes its own `RecordMonitor` streams in a separate session,
    /// so without this every absolute motion was dropped and clicks landed
    /// at the origin.
    ///
    /// # Errors
    ///
    /// [`MutterError`]; input keeps its previous targets.
    pub async fn record_input_monitors(&self, connectors: &[String]) -> Result<(), MutterError> {
        let cached = self
            .input_monitors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recorded
            .clone();
        let mut active = Vec::new();
        for connector in connectors {
            let reusable = match cached.iter().find(|(name, _)| name == connector) {
                // A stream whose monitor went away was closed by Mutter.
                Some((_, path)) if self.stream_exists(path).await => Some(path.clone()),
                _ => None,
            };
            let path = match reusable {
                Some(path) => path,
                None => {
                    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
                    options.insert("cursor-mode", Value::from(0_u32));
                    self.screen_cast_session
                        .call("RecordMonitor", &(connector.as_str(), options))
                        .await?
                }
            };
            active.push((connector.clone(), path));
        }
        self.input_monitors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .engage(active);
        Ok(())
    }

    /// Returns absolute input to the outputs' own streams. Idempotent.
    pub fn release_input_monitors(&self) {
        self.input_monitors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release();
    }

    async fn stream_exists(&self, path: &OwnedObjectPath) -> bool {
        let probe = async {
            let properties = Proxy::new(
                &self.connection,
                SCREEN_CAST,
                path.clone(),
                "org.freedesktop.DBus.Properties",
            )
            .await?;
            properties
                .call::<_, _, HashMap<String, OwnedValue>>("GetAll", &(SCREEN_CAST_STREAM,))
                .await
        };
        matches!(
            tokio::time::timeout(Duration::from_secs(2), probe).await,
            Ok(Ok(_))
        )
    }

    /// Whether the `RemoteDesktop` session offers `ConnectToEIS` (GNOME 45
    /// and later), from the session object's introspection. `None` when the
    /// introspection could not be read.
    pub async fn eis_offered(&self) -> Option<bool> {
        let introspectable = zbus::fdo::IntrospectableProxy::builder(&self.connection)
            .destination(REMOTE_DESKTOP)
            .ok()?
            .path(self.remote_desktop.path().clone())
            .ok()?
            .build()
            .await
            .ok()?;
        let xml = introspectable.introspect().await.ok()?;
        Some(xml.contains("name=\"ConnectToEIS\""))
    }

    /// Injects one validated input command.
    ///
    /// # Errors
    ///
    /// [`MutterError`] when `RemoteDesktop` refuses it.
    pub async fn notify(&self, command: &InputCommand) -> Result<(), MutterError> {
        let rd = &self.remote_desktop;
        match command {
            InputCommand::PointerAbsolute { output, x, y, .. } => {
                self.notify_absolute(AbsoluteCall::Pointer, output, (*x, *y))
                    .await?;
            }
            InputCommand::PointerRelative { dx, dy } => {
                rd.call::<_, _, ()>("NotifyPointerMotionRelative", &(*dx, *dy))
                    .await?;
            }
            InputCommand::PointerButton { button, pressed } => {
                rd.call::<_, _, ()>("NotifyPointerButton", &(*button, *pressed))
                    .await?;
            }
            InputCommand::PointerAxis {
                dx,
                dy,
                finish,
                source,
            } => {
                rd.call::<_, _, ()>(
                    "NotifyPointerAxis",
                    &(*dx, *dy, source.mutter_flags(*finish)),
                )
                .await?;
            }
            InputCommand::PointerAxisDiscrete { axis, steps } => {
                rd.call::<_, _, ()>("NotifyPointerAxisDiscrete", &(*axis, *steps))
                    .await?;
            }
            InputCommand::Key { keycode, pressed } => {
                rd.call::<_, _, ()>("NotifyKeyboardKeycode", &(*keycode, *pressed))
                    .await?;
            }
            InputCommand::Keysym { keysym, pressed } => {
                rd.call::<_, _, ()>("NotifyKeyboardKeysym", &(*keysym, *pressed))
                    .await?;
            }
            InputCommand::TouchDown {
                output, slot, x, y, ..
            } => {
                self.notify_absolute(AbsoluteCall::TouchDown(*slot), output, (*x, *y))
                    .await?;
            }
            InputCommand::TouchMotion {
                output, slot, x, y, ..
            } => {
                self.notify_absolute(AbsoluteCall::TouchMotion(*slot), output, (*x, *y))
                    .await?;
            }
            InputCommand::TouchUp { slot } => {
                rd.call::<_, _, ()>("NotifyTouchUp", &(*slot,)).await?;
            }
            // RemoteDesktop has no tablet device; a pen reaches the session
            // only through an emulated stylus, which this backend is not.
            InputCommand::Pen { .. } => {
                return Err(MutterError::NoStylus(
                    arcen_input::pen_emulation::StylusUnavailable::StylusNotOffered,
                ));
            }
            InputCommand::PenAway
            | InputCommand::Hello
            | InputCommand::ReleaseAll
            | InputCommand::Locks { .. }
            | InputCommand::Text { .. }
            | InputCommand::Keymap => {}
        }
        Ok(())
    }

    /// Asks the `RemoteDesktop` session for a connection to its compositor's
    /// EIS server (`ConnectToEIS`, GNOME 45 and later): a socket that speaks
    /// the libei protocol.
    ///
    /// # Errors
    ///
    /// [`MutterError::Bus`] when the compositor refuses or lacks it.
    pub async fn connect_to_eis(&self) -> Result<std::os::fd::OwnedFd, MutterError> {
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        let fd: zbus::zvariant::OwnedFd = self
            .remote_desktop
            .call("ConnectToEIS", &(options,))
            .await?;
        Ok(fd.into())
    }

    /// The `Closed` signals of both sessions as one owned stream, so a
    /// caller can wait for them without borrowing the sessions. `None` when
    /// they cannot be subscribed.
    pub async fn closed_signals(
        &self,
    ) -> Option<impl futures_util::Stream<Item = zbus::Message> + Send + Unpin + 'static> {
        let remote = self.remote_desktop.receive_signal("Closed").await.ok()?;
        let screen = self
            .screen_cast_session
            .receive_signal("Closed")
            .await
            .ok()?;
        Some(futures_util::stream::select(remote, screen))
    }

    /// Resolves when Mutter closes either session.
    pub async fn wait_closed(&self) {
        let remote = self.remote_desktop.receive_signal("Closed").await;
        let screen = self.screen_cast_session.receive_signal("Closed").await;
        match (remote, screen) {
            (Ok(mut remote), Ok(mut screen)) => {
                tokio::select! {
                    _ = remote.next() => {}
                    _ = screen.next() => {}
                }
            }
            _ => std::future::pending::<()>().await,
        }
    }

    /// Stops both sessions. Idempotent; a compositor that is already gone
    /// has closed them itself.
    pub async fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if let Some(follower) = self.geometry_follower.take() {
            follower.abort();
        }
        if let Err(error) = tokio::time::timeout(
            Duration::from_secs(2),
            self.remote_desktop.call::<_, _, ()>("Stop", &()),
        )
        .await
        .unwrap_or(Ok(()))
        {
            tracing::info!(
                target: target::SESSION,
                %error,
                "RemoteDesktop session stop reported an error; the compositor may already be gone"
            );
        }
        let _ = &self.connection;
    }
}

/// `RecordMonitor` streams of outputs that already exist, in a `ScreenCast`
/// session of their own (no `RemoteDesktop` link: input stays on the
/// paired sessions). A `RecordVirtual` stream offers eight-bit formats only,
/// whatever its monitor's colour state; recording the same monitor again
/// with `RecordMonitor` gives a consumer the compositor's wide formats in an
/// HDR colour state (Mutter 50 and later). Stopped explicitly: Mutter would
/// otherwise keep it until the agent's bus connection closes.
pub struct MonitorCasts {
    session: Proxy<'static>,
    streams: Vec<ReadyStream>,
    stopped: bool,
}

impl MonitorCasts {
    /// Records each of `outputs` (by connector, keeping its size and
    /// place) with `cursor_mode`, starts the session, and waits for every
    /// stream's `PipeWire` node.
    ///
    /// # Errors
    ///
    /// [`MutterError`]; a session that was created is stopped again.
    pub async fn start(
        connection: &Connection,
        outputs: &[ReadyStream],
        cursor_mode: u32,
    ) -> Result<Self, MutterError> {
        wait_for_name(connection, SCREEN_CAST).await?;
        let root = Proxy::new(connection, SCREEN_CAST, SCREEN_CAST_PATH, SCREEN_CAST).await?;
        let properties: HashMap<&str, Value<'_>> = HashMap::new();
        let path: OwnedObjectPath = root.call("CreateSession", &(properties,)).await?;
        let session = Proxy::new(connection, SCREEN_CAST, path, SCREEN_CAST_SESSION).await?;
        let mut casts = Self {
            session,
            streams: Vec::new(),
            stopped: false,
        };
        match casts.record(connection, outputs, cursor_mode).await {
            Ok(()) => Ok(casts),
            Err(error) => {
                casts.stop().await;
                Err(error)
            }
        }
    }

    async fn record(
        &mut self,
        connection: &Connection,
        outputs: &[ReadyStream],
        cursor_mode: u32,
    ) -> Result<(), MutterError> {
        let mut pending = Vec::new();
        for output in outputs {
            let mut options: HashMap<&str, Value<'_>> = HashMap::new();
            options.insert("cursor-mode", Value::from(cursor_mode));
            let stream_path: OwnedObjectPath = self
                .session
                .call("RecordMonitor", &(output.connector.as_str(), options))
                .await?;
            let stream =
                Proxy::new(connection, SCREEN_CAST, stream_path, SCREEN_CAST_STREAM).await?;
            let added = stream.receive_signal("PipeWireStreamAdded").await?;
            pending.push((output, added));
        }
        self.session.call::<_, _, ()>("Start", &()).await?;
        for (output, mut added) in pending {
            let message = tokio::time::timeout(STREAM_TIMEOUT, added.next())
                .await
                .ok()
                .flatten()
                .ok_or_else(|| MutterError::StreamTimeout(output.connector.clone()))?;
            let (node_id,): (u32,) = message.body().deserialize()?;
            tracing::info!(
                target: target::SESSION,
                connector = %output.connector,
                node_id,
                cursor_mode,
                "Mutter RecordMonitor stream for wide capture announced its PipeWire node"
            );
            self.streams.push(ReadyStream {
                node_id,
                ..output.clone()
            });
        }
        Ok(())
    }

    /// The streams, one per recorded output, in the order given.
    #[must_use]
    pub fn streams(&self) -> &[ReadyStream] {
        &self.streams
    }

    /// Stops the session; Mutter removes its streams. Idempotent.
    pub async fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if let Err(error) = tokio::time::timeout(
            Duration::from_secs(2),
            self.session.call::<_, _, ()>("Stop", &()),
        )
        .await
        .unwrap_or(Ok(()))
        {
            tracing::info!(
                target: target::SESSION,
                %error,
                "wide-capture ScreenCast session stop reported an error; the compositor may already be gone"
            );
        }
    }
}

impl CompositorInput for MutterSessions {
    type Error = MutterError;

    fn notify(
        &self,
        command: &InputCommand,
    ) -> impl std::future::Future<Output = Result<(), MutterError>> + Send {
        Self::notify(self, command)
    }
}

fn pair(parameters: &HashMap<String, OwnedValue>, key: &str) -> Result<(i32, i32), MutterError> {
    let value = parameters.get(key).ok_or(MutterError::Parameters)?;
    let value: Value<'static> = Value::from(value.try_clone()?);
    <(i32, i32)>::try_from(value).map_err(|_| MutterError::Parameters)
}

#[cfg(test)]
mod absolute_target_tests {
    use super::{absolute_target, AbsoluteInputStream, InputGeometry, InputMonitors, LayoutMode};
    use zbus::zvariant::OwnedObjectPath;

    fn path(value: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(value).unwrap()
    }

    fn active(monitors: &InputMonitors) -> Vec<(&str, &str)> {
        monitors
            .active
            .iter()
            .map(|(name, path)| (name.as_str(), path.as_str()))
            .collect()
    }

    #[test]
    fn a_single_outputs_closed_input_monitor_is_replaced_not_dropped() {
        let mut monitors = InputMonitors::default();
        monitors.engage(vec![("Meta-0".to_owned(), path("/m/0"))]);
        let epoch = monitors.forget(&path("/m/0")).expect("was addressed");
        // The only entry is gone while the replacement is recorded.
        assert!(monitors.active.is_empty());
        assert!(monitors.adopt("Meta-0", path("/m/0b"), epoch));
        assert_eq!(active(&monitors), [("Meta-0", "/m/0b")]);
        let own = [("Meta-0", path("/virtual/0"))];
        let own = own.iter().map(|(name, path)| (*name, path, VIRTUAL));
        assert_eq!(
            absolute_target(&monitors.active, own, "Meta-0"),
            Some((&path("/m/0b"), MONITOR, true))
        );
    }

    #[test]
    fn the_last_entry_of_several_is_replaced_in_place() {
        let mut monitors = InputMonitors::default();
        monitors.engage(vec![
            ("Meta-0".to_owned(), path("/m/0")),
            ("Meta-1".to_owned(), path("/m/1")),
        ]);
        let epoch = monitors.forget(&path("/m/1")).expect("was addressed");
        assert!(monitors.adopt("Meta-1", path("/m/1b"), epoch));
        assert_eq!(active(&monitors), [("Meta-0", "/m/0"), ("Meta-1", "/m/1b")]);
    }

    #[test]
    fn a_replacement_across_a_release_or_new_epoch_is_not_adopted() {
        let mut monitors = InputMonitors::default();
        monitors.engage(vec![("Meta-0".to_owned(), path("/m/0"))]);
        let epoch = monitors.forget(&path("/m/0")).expect("was addressed");
        monitors.release();
        assert!(!monitors.adopt("Meta-0", path("/m/0b"), epoch));
        assert!(monitors.active.is_empty());
        // Kept for reuse by the next epoch.
        assert_eq!(monitors.recorded.len(), 1);

        monitors.engage(vec![("Meta-0".to_owned(), path("/m/0c"))]);
        let stale = monitors.epoch;
        monitors.engage(vec![("Meta-0".to_owned(), path("/m/0d"))]);
        assert!(!monitors.adopt("Meta-0", path("/m/0e"), stale));
        assert_eq!(active(&monitors), [("Meta-0", "/m/0d")]);
    }

    #[test]
    fn a_stream_input_no_longer_addresses_is_only_forgotten() {
        let mut monitors = InputMonitors::default();
        monitors.engage(vec![("Meta-0".to_owned(), path("/m/0"))]);
        monitors.release();
        assert_eq!(monitors.forget(&path("/m/0")), None);
        assert!(monitors.recorded.is_empty());
    }

    const VIRTUAL: AbsoluteInputStream = AbsoluteInputStream::Virtual;
    const MONITOR: AbsoluteInputStream = AbsoluteInputStream::Monitor;

    #[test]
    fn an_input_monitor_wins_over_the_outputs_own_stream_only_for_its_connector() {
        let recorded = [("Meta-0", "/virtual/0"), ("Meta-1", "/virtual/1")];
        let own = || recorded.iter().map(|(name, path)| (*name, path, VIRTUAL));
        let none: [(String, &str); 0] = [];
        assert_eq!(
            absolute_target(&none, own(), "Meta-1"),
            Some((&"/virtual/1", VIRTUAL, false))
        );
        let monitors = [("Meta-0".to_owned(), "/monitor/0")];
        assert_eq!(
            absolute_target(&monitors, own(), "Meta-0"),
            Some((&"/monitor/0", MONITOR, true))
        );
        // Multi-output: an output without an input monitor keeps its stream.
        assert_eq!(
            absolute_target(&monitors, own(), "Meta-1"),
            Some((&"/virtual/1", VIRTUAL, false))
        );
        assert_eq!(absolute_target(&monitors, own(), "Meta-9"), None);
    }

    #[test]
    fn a_fixed_monitor_output_keeps_its_monitor_mapping() {
        let recorded = [("Virtual-1", "/monitor/own")];
        let own = || recorded.iter().map(|(name, path)| (*name, path, MONITOR));
        let none: [(String, &str); 0] = [];
        assert_eq!(
            absolute_target(&none, own(), "Virtual-1"),
            Some((&"/monitor/own", MONITOR, false))
        );
    }

    #[test]
    fn a_hidpi_virtual_output_in_the_logical_layout_takes_logical_pixels() {
        let geometry = InputGeometry {
            layout: LayoutMode::Logical,
            scales: vec![("Meta-0".to_owned(), 2.0), ("Meta-1".to_owned(), 1.0)],
        };
        assert_eq!(
            geometry.stream_coordinate(VIRTUAL, "Meta-0", (2880.0, 1800.0)),
            (1440.0, 900.0)
        );
        assert_eq!(
            geometry.stream_coordinate(VIRTUAL, "Meta-1", (100.0, 50.0)),
            (100.0, 50.0)
        );
        // Mutter divides a monitor stream's coordinate itself.
        assert_eq!(
            geometry.stream_coordinate(MONITOR, "Meta-0", (2880.0, 1800.0)),
            (2880.0, 1800.0)
        );
        // A monitor the layout does not report counts as unscaled.
        assert_eq!(
            geometry.stream_coordinate(VIRTUAL, "Meta-9", (10.0, 10.0)),
            (10.0, 10.0)
        );
    }

    #[test]
    fn the_physical_layout_takes_output_pixels_at_any_scale() {
        let geometry = InputGeometry {
            layout: LayoutMode::Physical,
            scales: vec![("Meta-0".to_owned(), 2.0)],
        };
        assert_eq!(
            geometry.stream_coordinate(VIRTUAL, "Meta-0", (2880.0, 1800.0)),
            (2880.0, 1800.0)
        );
        assert_eq!(InputGeometry::default().layout, LayoutMode::Physical);
    }
}
