//! Headless Wayland session readiness and capability-fact aggregation.
//!
//! The Linux Pier's headless GNOME/Mutter adapter gathers operating-system
//! facts — which compositor binaries advertise a headless mode, which
//! monitors Mutter reports, which `PipeWire` node a `ScreenCast` stream
//! announced, whether a `RemoteDesktop` session accepted input, whether a real
//! frame arrived — and hands them here. This module decides, as pure policy,
//! whether those facts add up to a session that may be announced ready.
//!
//! A running compositor, a `WAYLAND_DISPLAY` socket or a `PipeWire` node id
//! is never readiness by itself. Readiness is ordered and complete:
//!
//! 1. the compositor started;
//! 2. every requested virtual output exists with exactly the requested
//!    geometry and no other output exists;
//! 3. every output has a `ScreenCast` stream whose size equals the output;
//! 4. a `RemoteDesktop` session linked to that `ScreenCast` accepted absolute
//!    pointer input and offers keyboard and pointer;
//! 5. the capture side received a real frame from one of those streams, at
//!    the stream's size.
//!
//! Steps 1–4 are the session side's ([`WaylandSessionReadiness::session_side_ready`]);
//! step 5 comes from the capture consumer. Nothing here talks to D-Bus,
//! `PipeWire`, logind or a compositor.

use std::fmt::{Display, Formatter};

use crate::desktop_provider::{CapabilityFact, WaylandCapabilityFacts};

/// Largest virtual-output edge Arcen asks Mutter for.
pub const MAX_VIRTUAL_OUTPUT_EDGE: u32 = 16_384;

/// Largest refresh rate Arcen asks Mutter for.
pub const MAX_VIRTUAL_OUTPUT_REFRESH_HZ: u32 = 240;

/// A compositor binary Arcen can run as a headless session.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum HeadlessCompositor {
    /// `gnome-shell --headless`: the full GNOME desktop on Mutter.
    GnomeShell,
    /// `mutter --headless`: the bare window manager, no GNOME Shell UI.
    Mutter,
}

impl HeadlessCompositor {
    /// Stable token for logs and READY truth.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::GnomeShell => "gnome-shell",
            Self::Mutter => "mutter",
        }
    }

    /// The executable's file name inside a prefix's `bin`.
    #[must_use]
    pub const fn executable_name(self) -> &'static str {
        match self {
            Self::GnomeShell => "gnome-shell",
            Self::Mutter => "mutter",
        }
    }

    /// The executable in the distribution's `/usr`. A Pier configured with
    /// a side-by-side prefix uses
    /// [`crate::compositor_prefix::CompositorPrefix::binary`] instead.
    #[must_use]
    pub const fn binary(self) -> &'static str {
        match self {
            Self::GnomeShell => "/usr/bin/gnome-shell",
            Self::Mutter => "/usr/bin/mutter",
        }
    }
}

/// The compositors Arcen can run, in preference order.
pub const WAYLAND_HOST_COMPOSITORS: [HeadlessCompositor; 2] =
    [HeadlessCompositor::GnomeShell, HeadlessCompositor::Mutter];

/// Where systemd-logind keeps its seat state; present while it runs.
pub const LOGIND_SEATS_DIR: &str = "/run/systemd/seats";
/// The D-Bus daemon a headless session runs its private bus with.
pub const DBUS_DAEMON_BINARY: &str = "/usr/bin/dbus-daemon";
/// The `PipeWire` daemon a headless session runs.
pub const PIPEWIRE_BINARY: &str = "/usr/bin/pipewire";
/// The `PipeWire` session manager a headless session runs.
pub const WIREPLUMBER_BINARY: &str = "/usr/bin/wireplumber";

/// What `--help` says a compositor binary supports: headless mode and
/// virtual monitors.
#[must_use]
pub fn parse_compositor_help(help: &str) -> (bool, bool) {
    let has = |flag: &str| {
        help.lines()
            .any(|line| line.split_whitespace().next() == Some(flag))
    };
    (has("--headless"), has("--virtual-monitor"))
}

/// What one compositor binary advertised when probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositorProbe {
    /// Probed binary.
    pub compositor: HeadlessCompositor,
    /// It advertises a headless display-server mode.
    pub headless: bool,
    /// It advertises persistent virtual monitors.
    pub virtual_monitor: bool,
}

impl CompositorProbe {
    /// Whether this binary can host an Arcen headless session.
    #[must_use]
    pub const fn usable(&self) -> bool {
        self.headless && self.virtual_monitor
    }
}

/// Chooses the compositor to run: GNOME Shell when it is usable, otherwise
/// Mutter, otherwise none.
#[must_use]
pub fn choose_headless_compositor(probes: &[CompositorProbe]) -> Option<HeadlessCompositor> {
    WAYLAND_HOST_COMPOSITORS.into_iter().find(|preferred| {
        probes
            .iter()
            .any(|probe| probe.compositor == *preferred && probe.usable())
    })
}

/// Host facts a Linux Pier can establish without starting a session.
///
/// Each field is a separate fact. Package presence alone proves none of
/// them; the host adapter fills them from probes of the actual binaries,
/// services and device nodes.
// Each flag is an independent probe result the facts aggregate separately.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaylandHostProbe {
    /// Every compositor binary that was found and probed.
    pub compositors: Vec<CompositorProbe>,
    /// systemd-logind is running, so `pam_systemd` can create a session.
    pub logind: bool,
    /// A D-Bus daemon that can run a private session bus is executable.
    pub dbus_daemon: bool,
    /// `PipeWire` is executable.
    pub pipewire: bool,
    /// A `PipeWire` session manager (`WirePlumber`) is executable.
    pub session_manager: bool,
    /// Whether an unprivileged session user can open the encoder GPU's
    /// render node.
    pub render_node: CapabilityFact,
    /// Whether a capture route delivered an encoded first frame. The
    /// session adapter never establishes this itself.
    pub capture_route: CapabilityFact,
    /// Whether the capture side proved it imports compositor buffers on the
    /// GPU. The session adapter never establishes this itself.
    pub gpu_import: CapabilityFact,
    /// Whether session-bound input injection is proven.
    pub input_injection: CapabilityFact,
}

impl WaylandHostProbe {
    /// The compositor this host would run.
    #[must_use]
    pub fn compositor(&self) -> Option<HeadlessCompositor> {
        choose_headless_compositor(&self.compositors)
    }

    /// Aggregates the probes into the selection facts.
    #[must_use]
    pub fn facts(&self) -> WaylandCapabilityFacts {
        let present = |available: bool| {
            if available {
                CapabilityFact::Available
            } else {
                CapabilityFact::Missing
            }
        };
        WaylandCapabilityFacts {
            compositor: present(self.compositor().is_some()),
            session_service: present(self.logind && self.dbus_daemon),
            pipewire: present(self.pipewire && self.session_manager),
            render_node_access: self.render_node,
            capture_route: self.capture_route,
            gpu_import: self.gpu_import,
            input_injection: self.input_injection,
        }
    }
}

/// One requested virtual output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualOutputRequest {
    width: u32,
    height: u32,
    refresh_hz: u32,
}

/// A virtual-output request outside what Arcen asks a compositor for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualOutputRequestError {
    /// Width or height is zero or above [`MAX_VIRTUAL_OUTPUT_EDGE`].
    Size {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// Refresh is zero or above [`MAX_VIRTUAL_OUTPUT_REFRESH_HZ`].
    Refresh(u32),
}

impl Display for VirtualOutputRequestError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Size { width, height } => write!(
                formatter,
                "virtual output {width}x{height} is outside 1..={MAX_VIRTUAL_OUTPUT_EDGE}"
            ),
            Self::Refresh(refresh) => write!(
                formatter,
                "virtual output refresh {refresh} Hz is outside 1..={MAX_VIRTUAL_OUTPUT_REFRESH_HZ}"
            ),
        }
    }
}

impl std::error::Error for VirtualOutputRequestError {}

impl VirtualOutputRequest {
    /// A checked request.
    ///
    /// # Errors
    ///
    /// [`VirtualOutputRequestError`] for a zero or oversized edge or refresh.
    pub const fn new(
        width: u32,
        height: u32,
        refresh_hz: u32,
    ) -> Result<Self, VirtualOutputRequestError> {
        if width == 0
            || height == 0
            || width > MAX_VIRTUAL_OUTPUT_EDGE
            || height > MAX_VIRTUAL_OUTPUT_EDGE
        {
            return Err(VirtualOutputRequestError::Size { width, height });
        }
        if refresh_hz == 0 || refresh_hz > MAX_VIRTUAL_OUTPUT_REFRESH_HZ {
            return Err(VirtualOutputRequestError::Refresh(refresh_hz));
        }
        Ok(Self {
            width,
            height,
            refresh_hz,
        })
    }

    /// Requested width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Requested height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Requested refresh in hertz.
    #[must_use]
    pub const fn refresh_hz(&self) -> u32 {
        self.refresh_hz
    }
}

/// One monitor as the compositor reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedMonitor {
    /// Connector name, for example `Meta-0`.
    pub connector: String,
    /// Whether the compositor reports it as a virtual monitor.
    pub is_virtual: bool,
    /// The current mode, if any: width, height, refresh in millihertz.
    pub current_mode: Option<(u32, u32, u32)>,
}

/// A requested output that the compositor provably applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOutput {
    /// Connector name the compositor gave it.
    pub connector: String,
    /// Applied width in pixels.
    pub width: u32,
    /// Applied height in pixels.
    pub height: u32,
    /// Applied refresh in hertz.
    pub refresh_hz: u32,
}

/// Why the compositor's monitors do not match the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputVerifyError {
    /// A different number of monitors exists than was requested.
    Count {
        /// Requested outputs.
        requested: usize,
        /// Monitors the compositor reports.
        observed: usize,
    },
    /// A monitor is not a virtual monitor this session created.
    NotVirtual {
        /// Its connector.
        connector: String,
    },
    /// A monitor has no current mode.
    NoCurrentMode {
        /// Its connector.
        connector: String,
    },
    /// A monitor's current mode is not the requested one.
    Geometry {
        /// Its connector.
        connector: String,
        /// Requested width, height, refresh in hertz.
        expected: (u32, u32, u32),
        /// Observed width, height, refresh in millihertz.
        observed: (u32, u32, u32),
    },
}

impl Display for OutputVerifyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count {
                requested,
                observed,
            } => write!(
                formatter,
                "compositor reports {observed} monitors, {requested} were requested"
            ),
            Self::NotVirtual { connector } => {
                write!(formatter, "monitor {connector} is not a virtual monitor")
            }
            Self::NoCurrentMode { connector } => {
                write!(formatter, "monitor {connector} has no current mode")
            }
            Self::Geometry {
                connector,
                expected,
                observed,
            } => write!(
                formatter,
                "monitor {connector} runs {}x{}@{}mHz, {}x{}@{}Hz was requested",
                observed.0, observed.1, observed.2, expected.0, expected.1, expected.2
            ),
        }
    }
}

impl std::error::Error for OutputVerifyError {}

/// Refresh tolerance: compositors report fractional refresh rates.
const REFRESH_TOLERANCE_MILLIHZ: u32 = 500;

/// Checks the compositor's monitors against the request, in order.
///
/// The session owns its compositor, so every monitor must be one it asked
/// for: an extra or missing monitor, a physical monitor, or any geometry
/// other than the requested one is refused. Nothing is nearest-matched.
///
/// # Errors
///
/// [`OutputVerifyError`] naming the first mismatch.
pub fn verify_virtual_outputs(
    requested: &[VirtualOutputRequest],
    observed: &[ObservedMonitor],
) -> Result<Vec<VerifiedOutput>, OutputVerifyError> {
    if requested.len() != observed.len() {
        return Err(OutputVerifyError::Count {
            requested: requested.len(),
            observed: observed.len(),
        });
    }
    requested
        .iter()
        .zip(observed)
        .map(|(request, monitor)| {
            if !monitor.is_virtual {
                return Err(OutputVerifyError::NotVirtual {
                    connector: monitor.connector.clone(),
                });
            }
            let Some((width, height, refresh_millihz)) = monitor.current_mode else {
                return Err(OutputVerifyError::NoCurrentMode {
                    connector: monitor.connector.clone(),
                });
            };
            let expected_millihz = request.refresh_hz.saturating_mul(1000);
            if width != request.width
                || height != request.height
                || refresh_millihz.abs_diff(expected_millihz) > REFRESH_TOLERANCE_MILLIHZ
            {
                return Err(OutputVerifyError::Geometry {
                    connector: monitor.connector.clone(),
                    expected: (request.width, request.height, request.refresh_hz),
                    observed: (width, height, refresh_millihz),
                });
            }
            Ok(VerifiedOutput {
                connector: monitor.connector.clone(),
                width,
                height,
                refresh_hz: request.refresh_hz,
            })
        })
        .collect()
}

/// A `ScreenCast` stream the compositor announced for one output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCastStream {
    /// Output connector the stream records.
    pub connector: String,
    /// `PipeWire` node id from `PipeWireStreamAdded`.
    pub node_id: u32,
    /// Stream width from the stream's `Parameters`.
    pub width: u32,
    /// Stream height from the stream's `Parameters`.
    pub height: u32,
}

/// Mutter `RemoteDesktop` device-type bits (`SupportedDeviceTypes`).
pub mod device_type {
    /// Keyboard.
    pub const KEYBOARD: u32 = 1;
    /// Pointer.
    pub const POINTER: u32 = 2;
    /// Touchscreen.
    pub const TOUCHSCREEN: u32 = 4;
}

/// What the `RemoteDesktop` session proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteDesktopEvidence {
    /// `SupportedDeviceTypes` bitmask.
    pub device_types: u32,
    /// The session was started.
    pub started: bool,
    /// It is linked to the `ScreenCast` session whose stream it addresses.
    pub linked_to_screencast: bool,
    /// An absolute pointer motion addressed to a stream was accepted.
    pub absolute_pointer_accepted: bool,
}

/// Input a headless Mutter session can inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutterInputCapabilities {
    /// Keyboard keycodes and keysyms.
    pub keyboard: CapabilityFact,
    /// Absolute pointer, buttons and scroll.
    pub pointer: CapabilityFact,
    /// Touch.
    pub touch: CapabilityFact,
    /// Pressure-capable pen. Mutter `RemoteDesktop` has no tablet device type.
    pub pen: CapabilityFact,
    /// An EIS (libei) connection. Not offered through this D-Bus path.
    pub eis: CapabilityFact,
}

impl RemoteDesktopEvidence {
    /// The input this session can inject. Pen and EIS are never claimed by
    /// the `RemoteDesktop` D-Bus path.
    #[must_use]
    pub const fn capabilities(&self) -> MutterInputCapabilities {
        const fn has(types: u32, bit: u32) -> CapabilityFact {
            if types & bit != 0 {
                CapabilityFact::Available
            } else {
                CapabilityFact::Missing
            }
        }
        MutterInputCapabilities {
            keyboard: has(self.device_types, device_type::KEYBOARD),
            pointer: has(self.device_types, device_type::POINTER),
            touch: has(self.device_types, device_type::TOUCHSCREEN),
            pen: CapabilityFact::Missing,
            eis: CapabilityFact::Missing,
        }
    }
}

/// How a delivered frame's pixels were carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMemory {
    /// Shared memory (`MemFd`/`MemPtr`).
    SharedMemory,
    /// DMA-BUF.
    DmaBuf,
    /// The consumer did not report it.
    Unknown,
}

/// How the capture consumer moved a frame into its encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameImport {
    /// The consumer did not encode the frame, or did not say how.
    Unreported,
    /// Host memory copied into the encoder's input (`host-copy`).
    HostCopy,
    /// A same-GPU import followed by a GPU copy or conversion
    /// (`gpu-convert`).
    GpuConvert,
    /// A same-GPU import the encoder reads directly (`zero-copy-import`).
    ZeroCopyImport,
}

impl FrameImport {
    /// Whether the frame reached an encoder through a truthful, reported
    /// route (any route, including a labelled host copy).
    #[must_use]
    pub const fn delivered(self) -> bool {
        !matches!(self, Self::Unreported)
    }

    /// Whether the compositor's buffer was imported on the GPU without a
    /// host copy.
    #[must_use]
    pub const fn gpu_import(self) -> bool {
        matches!(self, Self::GpuConvert | Self::ZeroCopyImport)
    }
}

/// A real frame the capture consumer received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameEvidence {
    /// `PipeWire` node it came from.
    pub node_id: u32,
    /// Negotiated pixel format token, for example `BGRx`.
    pub format: String,
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Buffer memory.
    pub memory: FrameMemory,
    /// Route into the encoder.
    pub import: FrameImport,
}

/// A readiness step that has not been proven yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessGap {
    /// The compositor has not started.
    Compositor,
    /// Virtual outputs are not verified.
    Outputs,
    /// An output has no `ScreenCast` stream.
    Stream {
        /// The output's connector.
        connector: String,
    },
    /// No usable `RemoteDesktop` input target.
    Input,
    /// No real frame has been received.
    Frame,
}

impl ReadinessGap {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Compositor => "compositor",
            Self::Outputs => "outputs",
            Self::Stream { .. } => "screencast_stream",
            Self::Input => "input_target",
            Self::Frame => "first_frame",
        }
    }
}

/// Evidence recorded out of order or contradicting what is already known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessError {
    /// A step was recorded before the one it depends on.
    OutOfOrder {
        /// The missing earlier step.
        missing: ReadinessGap,
    },
    /// Evidence for a step that was already recorded.
    AlreadyRecorded(&'static str),
    /// A stream for an output this session did not verify.
    UnknownOutput {
        /// The stream's connector.
        connector: String,
    },
    /// A stream whose size is not its output's.
    StreamGeometry {
        /// The output's connector.
        connector: String,
        /// Output size.
        output: (u32, u32),
        /// Stream size.
        stream: (u32, u32),
    },
    /// A stream with node id zero.
    InvalidNode,
    /// A node id used by two streams.
    DuplicateNode(u32),
    /// The `RemoteDesktop` session is not started.
    InputNotStarted,
    /// The `RemoteDesktop` session is not linked to the `ScreenCast` session.
    InputNotLinked,
    /// The `RemoteDesktop` session lacks keyboard or pointer.
    InputDevices(u32),
    /// No absolute pointer motion was accepted.
    InputNotProbed,
    /// A frame from a node no stream announced.
    UnknownNode(u32),
    /// A frame whose size is not its stream's.
    FrameGeometry {
        /// Stream size.
        stream: (u32, u32),
        /// Frame size.
        frame: (u32, u32),
    },
    /// A frame with no format.
    FrameFormat,
}

impl Display for ReadinessError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfOrder { missing } => {
                write!(
                    formatter,
                    "readiness step {} is not proven yet",
                    missing.token()
                )
            }
            Self::AlreadyRecorded(step) => write!(formatter, "{step} was already recorded"),
            Self::UnknownOutput { connector } => {
                write!(formatter, "stream for unverified output {connector}")
            }
            Self::StreamGeometry {
                connector,
                output,
                stream,
            } => write!(
                formatter,
                "stream for {connector} is {}x{}, the output is {}x{}",
                stream.0, stream.1, output.0, output.1
            ),
            Self::InvalidNode => formatter.write_str("stream announced node id 0"),
            Self::DuplicateNode(node) => write!(formatter, "node {node} announced twice"),
            Self::InputNotStarted => formatter.write_str("RemoteDesktop session is not started"),
            Self::InputNotLinked => {
                formatter.write_str("RemoteDesktop session is not linked to the ScreenCast")
            }
            Self::InputDevices(types) => write!(
                formatter,
                "RemoteDesktop device types {types:#x} lack keyboard or pointer"
            ),
            Self::InputNotProbed => {
                formatter.write_str("no absolute pointer motion was accepted by RemoteDesktop")
            }
            Self::UnknownNode(node) => write!(formatter, "frame from unannounced node {node}"),
            Self::FrameGeometry { stream, frame } => write!(
                formatter,
                "frame is {}x{}, its stream is {}x{}",
                frame.0, frame.1, stream.0, stream.1
            ),
            Self::FrameFormat => formatter.write_str("frame has no pixel format"),
        }
    }
}

impl std::error::Error for ReadinessError {}

/// The evidence of a fully ready session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyEvidence {
    /// Compositor running the session.
    pub compositor: HeadlessCompositor,
    /// Verified outputs, in request order.
    pub outputs: Vec<VerifiedOutput>,
    /// One stream per output, in output order.
    pub streams: Vec<ScreenCastStream>,
    /// Input target evidence.
    pub input: RemoteDesktopEvidence,
    /// The first real frame.
    pub frame: FrameEvidence,
}

/// Ordered readiness of one headless Wayland session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaylandSessionReadiness {
    compositor: Option<HeadlessCompositor>,
    outputs: Option<Vec<VerifiedOutput>>,
    streams: Vec<ScreenCastStream>,
    input: Option<RemoteDesktopEvidence>,
    frame: Option<FrameEvidence>,
}

impl WaylandSessionReadiness {
    /// Nothing proven.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The compositor started.
    ///
    /// # Errors
    ///
    /// [`ReadinessError::AlreadyRecorded`] when recorded twice.
    pub fn record_compositor(
        &mut self,
        compositor: HeadlessCompositor,
    ) -> Result<(), ReadinessError> {
        if self.compositor.is_some() {
            return Err(ReadinessError::AlreadyRecorded("compositor"));
        }
        self.compositor = Some(compositor);
        Ok(())
    }

    /// The outputs were verified with [`verify_virtual_outputs`].
    ///
    /// # Errors
    ///
    /// Out of order, or recorded twice.
    pub fn record_outputs(&mut self, outputs: Vec<VerifiedOutput>) -> Result<(), ReadinessError> {
        if self.compositor.is_none() {
            return Err(ReadinessError::OutOfOrder {
                missing: ReadinessGap::Compositor,
            });
        }
        if self.outputs.is_some() {
            return Err(ReadinessError::AlreadyRecorded("outputs"));
        }
        if outputs.is_empty() {
            return Err(ReadinessError::OutOfOrder {
                missing: ReadinessGap::Outputs,
            });
        }
        self.outputs = Some(outputs);
        Ok(())
    }

    /// A `ScreenCast` stream was announced.
    ///
    /// # Errors
    ///
    /// Out of order, an unknown or already-streamed output, node id zero or
    /// reused, or a size different from its output.
    pub fn record_stream(&mut self, stream: ScreenCastStream) -> Result<(), ReadinessError> {
        let outputs = self.outputs.as_ref().ok_or(ReadinessError::OutOfOrder {
            missing: ReadinessGap::Outputs,
        })?;
        let output = outputs
            .iter()
            .find(|output| output.connector == stream.connector)
            .ok_or_else(|| ReadinessError::UnknownOutput {
                connector: stream.connector.clone(),
            })?;
        if stream.node_id == 0 {
            return Err(ReadinessError::InvalidNode);
        }
        if self
            .streams
            .iter()
            .any(|known| known.connector == stream.connector)
        {
            return Err(ReadinessError::AlreadyRecorded("stream"));
        }
        if self
            .streams
            .iter()
            .any(|known| known.node_id == stream.node_id)
        {
            return Err(ReadinessError::DuplicateNode(stream.node_id));
        }
        if (stream.width, stream.height) != (output.width, output.height) {
            return Err(ReadinessError::StreamGeometry {
                connector: stream.connector,
                output: (output.width, output.height),
                stream: (stream.width, stream.height),
            });
        }
        self.streams.push(stream);
        Ok(())
    }

    /// The `RemoteDesktop` input target was exercised.
    ///
    /// # Errors
    ///
    /// Out of order (every output needs its stream first), recorded twice,
    /// or evidence that does not prove a usable keyboard and absolute
    /// pointer.
    pub fn record_input(&mut self, input: RemoteDesktopEvidence) -> Result<(), ReadinessError> {
        if let Some(gap) = self.stream_gap() {
            return Err(ReadinessError::OutOfOrder { missing: gap });
        }
        if self.input.is_some() {
            return Err(ReadinessError::AlreadyRecorded("input"));
        }
        if !input.started {
            return Err(ReadinessError::InputNotStarted);
        }
        if !input.linked_to_screencast {
            return Err(ReadinessError::InputNotLinked);
        }
        let required = device_type::KEYBOARD | device_type::POINTER;
        if input.device_types & required != required {
            return Err(ReadinessError::InputDevices(input.device_types));
        }
        if !input.absolute_pointer_accepted {
            return Err(ReadinessError::InputNotProbed);
        }
        self.input = Some(input);
        Ok(())
    }

    /// The capture consumer received a real frame.
    ///
    /// # Errors
    ///
    /// Out of order, recorded twice, a frame from an unannounced node, a
    /// frame whose size is not its stream's, or no format.
    pub fn record_frame(&mut self, frame: FrameEvidence) -> Result<(), ReadinessError> {
        if !self.session_side_ready() {
            return Err(ReadinessError::OutOfOrder {
                missing: self
                    .gaps()
                    .into_iter()
                    .next()
                    .unwrap_or(ReadinessGap::Input),
            });
        }
        if self.frame.is_some() {
            return Err(ReadinessError::AlreadyRecorded("frame"));
        }
        let stream = self
            .streams
            .iter()
            .find(|stream| stream.node_id == frame.node_id)
            .ok_or(ReadinessError::UnknownNode(frame.node_id))?;
        if (frame.width, frame.height) != (stream.width, stream.height) {
            return Err(ReadinessError::FrameGeometry {
                stream: (stream.width, stream.height),
                frame: (frame.width, frame.height),
            });
        }
        if frame.format.trim().is_empty() {
            return Err(ReadinessError::FrameFormat);
        }
        self.frame = Some(frame);
        Ok(())
    }

    fn stream_gap(&self) -> Option<ReadinessGap> {
        if self.compositor.is_none() {
            return Some(ReadinessGap::Compositor);
        }
        let Some(outputs) = self.outputs.as_ref() else {
            return Some(ReadinessGap::Outputs);
        };
        outputs
            .iter()
            .find(|output| {
                !self
                    .streams
                    .iter()
                    .any(|stream| stream.connector == output.connector)
            })
            .map(|output| ReadinessGap::Stream {
                connector: output.connector.clone(),
            })
    }

    /// Every step not proven yet, in order.
    #[must_use]
    pub fn gaps(&self) -> Vec<ReadinessGap> {
        let mut gaps = Vec::new();
        if self.compositor.is_none() {
            gaps.push(ReadinessGap::Compositor);
        }
        match self.outputs.as_ref() {
            None => gaps.push(ReadinessGap::Outputs),
            Some(outputs) => gaps.extend(
                outputs
                    .iter()
                    .filter(|output| {
                        !self
                            .streams
                            .iter()
                            .any(|stream| stream.connector == output.connector)
                    })
                    .map(|output| ReadinessGap::Stream {
                        connector: output.connector.clone(),
                    }),
            ),
        }
        if self.input.is_none() {
            gaps.push(ReadinessGap::Input);
        }
        if self.frame.is_none() {
            gaps.push(ReadinessGap::Frame);
        }
        gaps
    }

    /// Steps 1–4 are proven: the session can be handed to a capture
    /// consumer, but must not be announced ready to a Deck yet.
    #[must_use]
    pub fn session_side_ready(&self) -> bool {
        self.gaps()
            .iter()
            .all(|gap| matches!(gap, ReadinessGap::Frame))
    }

    /// The complete evidence, or every missing step.
    ///
    /// # Errors
    ///
    /// Every [`ReadinessGap`] still open.
    pub fn ready(&self) -> Result<ReadyEvidence, Vec<ReadinessGap>> {
        match (
            self.compositor,
            self.outputs.as_ref(),
            self.input,
            self.frame.as_ref(),
        ) {
            (Some(compositor), Some(outputs), Some(input), Some(frame))
                if self.gaps().is_empty() =>
            {
                let streams = outputs
                    .iter()
                    .filter_map(|output| {
                        self.streams
                            .iter()
                            .find(|stream| stream.connector == output.connector)
                            .cloned()
                    })
                    .collect();
                Ok(ReadyEvidence {
                    compositor,
                    outputs: outputs.clone(),
                    streams,
                    input,
                    frame: frame.clone(),
                })
            }
            _ => Err(self.gaps()),
        }
    }

    /// Upgrades host facts with what this session proved. Session evidence
    /// only ever proves availability; it never hides a host fact that is
    /// proven missing. A first frame the capture consumer encoded through a
    /// reported route proves the capture route for this session; only a GPU
    /// import (`gpu-convert`, `zero-copy-import`) proves GPU import, never a
    /// host copy; a frame seen without an encoder (a probe) proves neither.
    #[must_use]
    pub fn proven_facts(&self, host: WaylandCapabilityFacts) -> WaylandCapabilityFacts {
        let mut facts = host;
        if self.outputs.is_some() {
            facts.compositor = CapabilityFact::Available;
            facts.session_service = CapabilityFact::Available;
        }
        if let Some(frame) = self.frame.as_ref() {
            facts.pipewire = CapabilityFact::Available;
            if frame.import.delivered() && facts.capture_route != CapabilityFact::Missing {
                facts.capture_route = CapabilityFact::Available;
            }
            if frame.import.gpu_import() && facts.gpu_import != CapabilityFact::Missing {
                facts.gpu_import = CapabilityFact::Available;
            }
        }
        if self.input.is_some() {
            facts.input_injection = CapabilityFact::Available;
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(width: u32, height: u32) -> VirtualOutputRequest {
        VirtualOutputRequest::new(width, height, 60).unwrap()
    }

    fn monitor(connector: &str, mode: Option<(u32, u32, u32)>) -> ObservedMonitor {
        ObservedMonitor {
            connector: connector.into(),
            is_virtual: true,
            current_mode: mode,
        }
    }

    fn input() -> RemoteDesktopEvidence {
        RemoteDesktopEvidence {
            device_types: 7,
            started: true,
            linked_to_screencast: true,
            absolute_pointer_accepted: true,
        }
    }

    fn stream(connector: &str, node: u32, width: u32, height: u32) -> ScreenCastStream {
        ScreenCastStream {
            connector: connector.into(),
            node_id: node,
            width,
            height,
        }
    }

    fn frame(node: u32, width: u32, height: u32) -> FrameEvidence {
        FrameEvidence {
            node_id: node,
            format: "BGRx".into(),
            width,
            height,
            memory: FrameMemory::SharedMemory,
            import: FrameImport::Unreported,
        }
    }

    fn session_side() -> WaylandSessionReadiness {
        let mut readiness = WaylandSessionReadiness::new();
        readiness
            .record_compositor(HeadlessCompositor::GnomeShell)
            .unwrap();
        let outputs = verify_virtual_outputs(
            &[request(1920, 1080)],
            &[monitor("Meta-0", Some((1920, 1080, 60_000)))],
        )
        .unwrap();
        readiness.record_outputs(outputs).unwrap();
        readiness
            .record_stream(stream("Meta-0", 44, 1920, 1080))
            .unwrap();
        readiness.record_input(input()).unwrap();
        readiness
    }

    #[test]
    fn gnome_shell_is_preferred_and_needs_both_flags() {
        let probes = [
            CompositorProbe {
                compositor: HeadlessCompositor::Mutter,
                headless: true,
                virtual_monitor: true,
            },
            CompositorProbe {
                compositor: HeadlessCompositor::GnomeShell,
                headless: true,
                virtual_monitor: true,
            },
        ];
        assert_eq!(
            choose_headless_compositor(&probes),
            Some(HeadlessCompositor::GnomeShell)
        );
        let no_virtual = [
            CompositorProbe {
                compositor: HeadlessCompositor::GnomeShell,
                headless: true,
                virtual_monitor: false,
            },
            probes[0],
        ];
        assert_eq!(
            choose_headless_compositor(&no_virtual),
            Some(HeadlessCompositor::Mutter)
        );
        assert_eq!(choose_headless_compositor(&no_virtual[..1]), None);
    }

    #[test]
    fn host_facts_are_separate_and_never_prove_import() {
        let probe = WaylandHostProbe {
            compositors: vec![CompositorProbe {
                compositor: HeadlessCompositor::GnomeShell,
                headless: true,
                virtual_monitor: true,
            }],
            logind: true,
            dbus_daemon: true,
            pipewire: true,
            session_manager: false,
            render_node: CapabilityFact::Available,
            capture_route: CapabilityFact::Unknown,
            gpu_import: CapabilityFact::Unknown,
            input_injection: CapabilityFact::Unknown,
        };
        let facts = probe.facts();
        assert_eq!(facts.compositor, CapabilityFact::Available);
        assert_eq!(facts.session_service, CapabilityFact::Available);
        assert_eq!(facts.pipewire, CapabilityFact::Missing);
        assert_eq!(facts.capture_route, CapabilityFact::Unknown);
        assert_eq!(facts.gpu_import, CapabilityFact::Unknown);
        assert_eq!(
            WaylandHostProbe::default().facts().compositor,
            CapabilityFact::Missing
        );
    }

    #[test]
    fn requests_are_bounded() {
        assert!(VirtualOutputRequest::new(0, 1080, 60).is_err());
        assert!(VirtualOutputRequest::new(16_385, 1080, 60).is_err());
        assert!(VirtualOutputRequest::new(1920, 1080, 0).is_err());
        assert!(VirtualOutputRequest::new(1920, 1080, 241).is_err());
        assert_eq!(request(1920, 1080).refresh_hz(), 60);
    }

    #[test]
    fn outputs_must_match_exactly_and_only_virtual() {
        let wanted = [request(1920, 1080)];
        assert!(matches!(
            verify_virtual_outputs(&wanted, &[]),
            Err(OutputVerifyError::Count { .. })
        ));
        assert!(matches!(
            verify_virtual_outputs(
                &wanted,
                &[
                    monitor("Meta-0", Some((1920, 1080, 60_000))),
                    monitor("Meta-1", Some((1920, 1080, 60_000)))
                ]
            ),
            Err(OutputVerifyError::Count { .. })
        ));
        assert!(matches!(
            verify_virtual_outputs(&wanted, &[monitor("Meta-0", Some((1280, 720, 60_000)))]),
            Err(OutputVerifyError::Geometry { .. })
        ));
        assert!(matches!(
            verify_virtual_outputs(&wanted, &[monitor("Meta-0", Some((1920, 1080, 30_000)))]),
            Err(OutputVerifyError::Geometry { .. })
        ));
        assert!(matches!(
            verify_virtual_outputs(&wanted, &[monitor("Meta-0", None)]),
            Err(OutputVerifyError::NoCurrentMode { .. })
        ));
        let mut physical = monitor("DP-1", Some((1920, 1080, 60_000)));
        physical.is_virtual = false;
        assert!(matches!(
            verify_virtual_outputs(&wanted, &[physical]),
            Err(OutputVerifyError::NotVirtual { .. })
        ));
        let verified =
            verify_virtual_outputs(&wanted, &[monitor("Meta-0", Some((1920, 1080, 59_940)))])
                .unwrap();
        assert_eq!(verified[0].connector, "Meta-0");
        assert_eq!(verified[0].refresh_hz, 60);
    }

    #[test]
    fn readiness_is_ordered() {
        let mut readiness = WaylandSessionReadiness::new();
        assert!(matches!(
            readiness.record_outputs(vec![]),
            Err(ReadinessError::OutOfOrder {
                missing: ReadinessGap::Compositor
            })
        ));
        assert!(matches!(
            readiness.record_stream(stream("Meta-0", 1, 1, 1)),
            Err(ReadinessError::OutOfOrder { .. })
        ));
        assert!(matches!(
            readiness.record_input(input()),
            Err(ReadinessError::OutOfOrder { .. })
        ));
        assert!(matches!(
            readiness.record_frame(frame(1, 1, 1)),
            Err(ReadinessError::OutOfOrder { .. })
        ));
        assert_eq!(readiness.gaps().len(), 4);
        assert!(!readiness.session_side_ready());
    }

    #[test]
    fn a_running_compositor_and_stream_are_not_readiness() {
        let mut readiness = WaylandSessionReadiness::new();
        readiness
            .record_compositor(HeadlessCompositor::Mutter)
            .unwrap();
        readiness
            .record_outputs(vec![VerifiedOutput {
                connector: "Meta-0".into(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }])
            .unwrap();
        readiness
            .record_stream(stream("Meta-0", 44, 1920, 1080))
            .unwrap();
        assert!(!readiness.session_side_ready());
        assert_eq!(
            readiness.ready().unwrap_err(),
            vec![ReadinessGap::Input, ReadinessGap::Frame]
        );
    }

    #[test]
    fn streams_must_match_their_output() {
        let mut readiness = WaylandSessionReadiness::new();
        readiness
            .record_compositor(HeadlessCompositor::GnomeShell)
            .unwrap();
        readiness
            .record_outputs(vec![VerifiedOutput {
                connector: "Meta-0".into(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }])
            .unwrap();
        assert!(matches!(
            readiness.record_stream(stream("Meta-1", 44, 1920, 1080)),
            Err(ReadinessError::UnknownOutput { .. })
        ));
        assert!(matches!(
            readiness.record_stream(stream("Meta-0", 0, 1920, 1080)),
            Err(ReadinessError::InvalidNode)
        ));
        assert!(matches!(
            readiness.record_stream(stream("Meta-0", 44, 1280, 720)),
            Err(ReadinessError::StreamGeometry { .. })
        ));
        readiness
            .record_stream(stream("Meta-0", 44, 1920, 1080))
            .unwrap();
        assert!(matches!(
            readiness.record_stream(stream("Meta-0", 45, 1920, 1080)),
            Err(ReadinessError::AlreadyRecorded(_))
        ));
    }

    #[test]
    fn input_must_be_started_linked_and_probed() {
        let base = session_side();
        let mut readiness = WaylandSessionReadiness {
            input: None,
            ..base
        };
        for (evidence, expected) in [
            (
                RemoteDesktopEvidence {
                    started: false,
                    ..input()
                },
                ReadinessError::InputNotStarted,
            ),
            (
                RemoteDesktopEvidence {
                    linked_to_screencast: false,
                    ..input()
                },
                ReadinessError::InputNotLinked,
            ),
            (
                RemoteDesktopEvidence {
                    device_types: device_type::POINTER,
                    ..input()
                },
                ReadinessError::InputDevices(device_type::POINTER),
            ),
            (
                RemoteDesktopEvidence {
                    absolute_pointer_accepted: false,
                    ..input()
                },
                ReadinessError::InputNotProbed,
            ),
        ] {
            assert_eq!(readiness.record_input(evidence), Err(expected));
        }
        readiness.record_input(input()).unwrap();
        let capabilities = input().capabilities();
        assert_eq!(capabilities.keyboard, CapabilityFact::Available);
        assert_eq!(capabilities.touch, CapabilityFact::Available);
        assert_eq!(capabilities.pen, CapabilityFact::Missing);
        assert_eq!(capabilities.eis, CapabilityFact::Missing);
    }

    #[test]
    fn ready_needs_a_real_frame_from_an_announced_stream() {
        let mut readiness = session_side();
        assert!(readiness.session_side_ready());
        assert_eq!(readiness.ready().unwrap_err(), vec![ReadinessGap::Frame]);
        assert_eq!(
            readiness.record_frame(frame(45, 1920, 1080)),
            Err(ReadinessError::UnknownNode(45))
        );
        assert!(matches!(
            readiness.record_frame(frame(44, 1280, 720)),
            Err(ReadinessError::FrameGeometry { .. })
        ));
        let mut formatless = frame(44, 1920, 1080);
        formatless.format = String::new();
        assert_eq!(
            readiness.record_frame(formatless),
            Err(ReadinessError::FrameFormat)
        );
        readiness.record_frame(frame(44, 1920, 1080)).unwrap();
        let ready = readiness.ready().unwrap();
        assert_eq!(ready.streams[0].node_id, 44);
        assert_eq!(ready.frame.format, "BGRx");
        assert_eq!(ready.compositor, HeadlessCompositor::GnomeShell);
    }

    #[test]
    fn a_frame_without_an_encoder_never_proves_import() {
        let mut readiness = session_side();
        readiness.record_frame(frame(44, 1920, 1080)).unwrap();
        let facts = readiness.proven_facts(WaylandCapabilityFacts::default());
        assert_eq!(facts.compositor, CapabilityFact::Available);
        assert_eq!(facts.pipewire, CapabilityFact::Available);
        assert_eq!(facts.input_injection, CapabilityFact::Available);
        assert_eq!(facts.capture_route, CapabilityFact::Unknown);
        assert_eq!(facts.gpu_import, CapabilityFact::Unknown);
        assert_eq!(facts.render_node_access, CapabilityFact::Unknown);
    }

    #[test]
    fn an_encoded_first_frame_proves_the_capture_route_and_only_a_gpu_import_proves_import() {
        for (import, gpu_import) in [
            (FrameImport::HostCopy, CapabilityFact::Unknown),
            (FrameImport::GpuConvert, CapabilityFact::Available),
            (FrameImport::ZeroCopyImport, CapabilityFact::Available),
        ] {
            let mut readiness = session_side();
            readiness
                .record_frame(FrameEvidence {
                    import,
                    ..frame(44, 1920, 1080)
                })
                .unwrap();
            let facts = readiness.proven_facts(WaylandCapabilityFacts::default());
            assert_eq!(facts.capture_route, CapabilityFact::Available, "{import:?}");
            assert_eq!(facts.gpu_import, gpu_import, "{import:?}");
            assert_eq!(facts.render_node_access, CapabilityFact::Unknown);
            // A host fact proven missing is never hidden by a session.
            let missing = readiness.proven_facts(WaylandCapabilityFacts {
                capture_route: CapabilityFact::Missing,
                gpu_import: CapabilityFact::Missing,
                ..WaylandCapabilityFacts::default()
            });
            assert_eq!(missing.capture_route, CapabilityFact::Missing);
            assert_eq!(missing.gpu_import, CapabilityFact::Missing);
        }
        // Proven per session: selection still needs every host fact.
        assert!(
            crate::desktop_provider::select_desktop_provider(
                crate::desktop_provider::ProviderPreference::Wayland,
                &crate::desktop_provider::HostDesktopFacts {
                    wayland_built: true,
                    wayland: WaylandCapabilityFacts {
                        capture_route: CapabilityFact::Available,
                        gpu_import: CapabilityFact::Available,
                        ..WaylandCapabilityFacts::default()
                    },
                    ..crate::desktop_provider::HostDesktopFacts::default()
                },
            )
            .is_err()
        );
    }
}
