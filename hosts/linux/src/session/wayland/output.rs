//! Mutter virtual outputs as an [`arcen_outputs::OutputProvider`].
//!
//! The headless compositor's outputs exist for exactly as long as the
//! compositor runs, so — like the dedicated Xorg provider — binding starts
//! the compositor stack, verification proves the monitors Mutter reports are
//! exactly the requested ones, and rollback stops the stack, which removes
//! every output it created. Nothing on the console or any other session is
//! touched.
//!
//! In the startup-monitor mode the outputs are `--virtual-monitor`
//! arguments. In the remote-monitor mode (Mutter 49+) the compositor starts
//! with none and binding also creates the paired `RemoteDesktop`/`ScreenCast`
//! session whose `RecordVirtual` streams own the outputs
//! ([`super::remote`]); verification then also proves their placement.

use std::path::{Path, PathBuf};

use arcen_media::MAX_MULTI_MONITOR_COUNT;
use arcen_outputs::{
    BindFailure, OutputCapabilities, OutputContext, OutputDemand, OutputProvider, OutputSurface,
    RollbackGuarantee,
};
use arcen_session::wayland_outputs::{
    validate_layout, LayoutError, PlacedOutput, VirtualOutputMode,
};
use arcen_session::wayland_session::{
    verify_virtual_outputs, HeadlessCompositor, OutputVerifyError, VerifiedOutput,
    VirtualOutputRequest,
};
use arcen_telemetry::CorrelationId;
use thiserror::Error;
use zbus::Connection;

use super::remote::{self, RemoteOutputError, RemoteSet};
use super::stack::{HeadlessStack, StackError};
use crate::logging::target;

/// Regions one startup-monitor session serves: `--virtual-monitor` outputs
/// cannot be placed, so one is all that mode promises exactly. A
/// remote-monitor session places its outputs and serves
/// [`VirtualOutputMode::max_outputs`].
pub const MUTTER_VIRTUAL_MAX_REGIONS: usize = 1;

/// What the headless Mutter provider promises.
///
/// - `Virtual`: every monitor is a compositor virtual monitor that did not
///   exist before bind.
/// - `exact_modes`: [`verify_virtual_outputs`] refuses any other mode.
/// - `persistent_dedicated_desktop`, `headless_capable`: the compositor runs
///   for the whole session with no physical output.
/// - `ExactRestore`: rollback stops the compositor stack, which removes its
///   monitors, and proves no session process or runtime directory remains.
///
/// Rotation, fractional scale and signed coordinates are not promised.
pub const MUTTER_VIRTUAL_CAPABILITIES: OutputCapabilities = {
    let mut capabilities = match OutputCapabilities::new(
        1,
        MUTTER_VIRTUAL_MAX_REGIONS,
        OutputSurface::Virtual,
        RollbackGuarantee::ExactRestore,
    ) {
        Ok(capabilities) => capabilities,
        Err(_) => panic!("headless Mutter serves exactly one region"),
    };
    capabilities.exact_modes = true;
    capabilities.persistent_dedicated_desktop = true;
    capabilities.headless_capable = true;
    capabilities
};

const _: () = assert!(MUTTER_VIRTUAL_MAX_REGIONS <= MAX_MULTI_MONITOR_COUNT);
const _: () =
    assert!(arcen_session::wayland_outputs::MAX_WAYLAND_OUTPUTS <= MAX_MULTI_MONITOR_COUNT);

/// What a provider in `mode` promises: the startup-monitor capabilities,
/// with as many regions as the mode places.
#[must_use]
pub fn capabilities_for(mode: VirtualOutputMode) -> OutputCapabilities {
    let mut capabilities = MUTTER_VIRTUAL_CAPABILITIES;
    if let Ok(mut placed) = OutputCapabilities::new(
        1,
        mode.max_outputs(),
        OutputSurface::Virtual,
        RollbackGuarantee::ExactRestore,
    ) {
        placed.exact_modes = true;
        placed.persistent_dedicated_desktop = true;
        placed.headless_capable = true;
        capabilities = placed;
    }
    capabilities
}

/// A headless Mutter output failure.
#[derive(Debug, Error)]
pub enum MutterOutputError {
    #[error("compositor binary {0} is unavailable")]
    CompositorUnavailable(String),
    #[error("private runtime directory {0} already exists")]
    RuntimeDirExists(PathBuf),
    #[error("headless stack failed: {0}")]
    Stack(#[from] StackError),
    #[error("compositor D-Bus failed: {0}")]
    Bus(#[from] super::mutter::MutterError),
    #[error("virtual outputs do not match the request: {0}")]
    Verify(#[from] OutputVerifyError),
    #[error("compositor exited before its outputs were verified")]
    CompositorExited,
    #[error("virtual output layout refused: {0}")]
    Layout(#[from] LayoutError),
    #[error("remote outputs failed: {0}")]
    Remote(#[from] RemoteOutputError),
}

/// Inert result of preflight.
pub struct PreparedMutterOutputs {
    outputs: Vec<PlacedOutput>,
    session_log_id: CorrelationId,
}

/// What a committed headless Mutter session produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutterOutputEvidence {
    /// Compositor that runs.
    pub compositor: HeadlessCompositor,
    /// Verified outputs; empty until verification succeeded.
    pub outputs: Vec<VerifiedOutput>,
    /// Private runtime directory.
    pub runtime_dir: PathBuf,
    /// Compositor process id.
    pub pid: u32,
}

/// Owns the compositor stack and the bus connection used to verify it.
pub struct MutterOutputBinding {
    stack: HeadlessStack,
    connection: Option<Connection>,
    requested: Vec<PlacedOutput>,
    remote: Option<RemoteSet>,
    evidence: MutterOutputEvidence,
    session_log_id: CorrelationId,
    armed: bool,
}

impl MutterOutputBinding {
    /// The private-bus connection verification opened.
    #[must_use]
    pub const fn connection(&self) -> Option<&Connection> {
        self.connection.as_ref()
    }

    /// The running stack.
    pub fn stack_mut(&mut self) -> &mut HeadlessStack {
        &mut self.stack
    }

    /// The verified outputs.
    #[must_use]
    pub fn outputs(&self) -> &[VerifiedOutput] {
        &self.evidence.outputs
    }

    /// Takes the remote-monitor session that owns the outputs (remote mode
    /// only). The caller then owns stopping it; rollback still stops the
    /// stack, which ends it in any case.
    pub fn take_remote(&mut self) -> Option<RemoteSet> {
        self.remote.take()
    }
}

/// Headless Mutter virtual-output provider for one logind session.
pub struct MutterVirtualOutputProvider {
    compositor: HeadlessCompositor,
    mode: VirtualOutputMode,
    runtime_dir: PathBuf,
    session_id: String,
    x11: bool,
    audio: super::audio::StackAudio,
    cursor_mode: super::StreamCursorMode,
    /// The GNOME login services the stack starts (taken by the first bind).
    services: Option<super::session_services::SessionServicesConfig>,
    colour: super::stack::CompositorColour,
}

impl MutterVirtualOutputProvider {
    /// A provider that will run `compositor` with outputs of `mode` in
    /// `runtime_dir` for logind session `session_id`.
    #[must_use]
    pub fn new(
        compositor: HeadlessCompositor,
        mode: VirtualOutputMode,
        runtime_dir: &Path,
        session_id: &str,
    ) -> Self {
        Self {
            compositor,
            mode,
            runtime_dir: runtime_dir.to_path_buf(),
            session_id: session_id.to_owned(),
            audio: super::audio::StackAudio::None,
            cursor_mode: super::StreamCursorMode::Embedded,
            services: None,
            colour: super::stack::CompositorColour::Default,
            x11: false,
        }
    }

    /// Runs the compositor with its Xwayland, for X11 applications.
    #[must_use]
    pub const fn with_x11(mut self, x11: bool) -> Self {
        self.x11 = x11;
        self
    }

    /// The same provider whose stack also carries the session's audio.
    #[must_use]
    pub fn with_audio(mut self, audio: super::audio::StackAudio) -> Self {
        self.audio = audio;
        self
    }

    /// The same provider, starting its compositor in `colour`.
    #[must_use]
    pub const fn with_colour(mut self, colour: super::stack::CompositorColour) -> Self {
        self.colour = colour;
        self
    }

    /// Records the remote monitors' streams with `cursor_mode`.
    #[must_use]
    pub const fn with_cursor_mode(mut self, cursor_mode: super::StreamCursorMode) -> Self {
        self.cursor_mode = cursor_mode;
        self
    }

    /// The stack this provider starts also runs the session's GNOME login
    /// services with `services`.
    #[must_use]
    pub fn with_services(
        mut self,
        services: super::session_services::SessionServicesConfig,
    ) -> Self {
        self.services = Some(services);
        self
    }

    /// The output mode.
    #[must_use]
    pub const fn mode(&self) -> VirtualOutputMode {
        self.mode
    }
}

impl OutputProvider for MutterVirtualOutputProvider {
    type Plan = Vec<PlacedOutput>;
    type Prepared = PreparedMutterOutputs;
    type Binding = MutterOutputBinding;
    type Evidence = MutterOutputEvidence;
    type Error = MutterOutputError;

    fn capabilities(&self) -> OutputCapabilities {
        capabilities_for(self.mode)
    }

    fn demand(&self, plan: &Self::Plan) -> OutputDemand {
        OutputDemand {
            exact_modes: true,
            persistent_desktop: true,
            headless: true,
            ..OutputDemand::new(plan.len())
        }
    }

    fn preflight(
        &mut self,
        plan: &Self::Plan,
        context: &OutputContext,
    ) -> Result<Self::Prepared, Self::Error> {
        let binary = super::compositor_binary(self.compositor);
        if !Path::new(&binary).is_file() {
            return Err(MutterOutputError::CompositorUnavailable(binary));
        }
        if self.runtime_dir.exists() {
            return Err(MutterOutputError::RuntimeDirExists(
                self.runtime_dir.clone(),
            ));
        }
        validate_layout(plan, self.mode)?;
        Ok(PreparedMutterOutputs {
            outputs: plan.clone(),
            session_log_id: context.session_log_id().clone(),
        })
    }

    /// The stack's own `Drop` kills every member and sweeps the session
    /// scope, so a cancelled or failed bind strands nothing.
    async fn bind(
        &mut self,
        prepared: Self::Prepared,
    ) -> Result<Self::Binding, BindFailure<Self::Error>> {
        let requests = prepared
            .outputs
            .iter()
            .map(|placed| placed.request)
            .collect::<Vec<VirtualOutputRequest>>();
        let stack = HeadlessStack::start(
            &self.runtime_dir,
            &self.session_id,
            self.compositor,
            &requests,
            self.mode,
            &self.audio,
            self.colour,
            self.x11,
            self.services.take(),
        )
        .await
        .map_err(|source| BindFailure {
            source: MutterOutputError::Stack(source),
            rollback: None,
        })?;
        let failed = |source: MutterOutputError| BindFailure {
            source,
            rollback: None,
        };
        // A failure below drops the stack, whose `Drop` kills every member.
        let (connection, remote) = match self.mode {
            VirtualOutputMode::StartupMonitors => (None, None),
            VirtualOutputMode::RemoteMonitors => {
                let connection = super::mutter::connect(&stack.bus_address())
                    .await
                    .map_err(|error| failed(error.into()))?;
                super::mutter::wait_for_name(&connection, "org.gnome.Mutter.DisplayConfig")
                    .await
                    .map_err(|error| failed(error.into()))?;
                let remote = remote::establish(
                    &connection,
                    &self.runtime_dir,
                    &prepared.outputs,
                    self.cursor_mode,
                )
                .await
                .map_err(|error| failed(error.into()))?;
                (Some(connection), Some(remote))
            }
        };
        let evidence = MutterOutputEvidence {
            compositor: self.compositor,
            outputs: Vec::new(),
            runtime_dir: self.runtime_dir.clone(),
            pid: stack.compositor_pid().unwrap_or_default(),
        };
        Ok(MutterOutputBinding {
            stack,
            connection,
            remote,
            requested: prepared.outputs,
            evidence,
            session_log_id: prepared.session_log_id,
            armed: true,
        })
    }

    async fn verify(&mut self, binding: &mut Self::Binding) -> Result<(), Self::Error> {
        if let (Some(connection), Some(set)) =
            (binding.connection.as_ref(), binding.remote.as_ref())
        {
            let verified = remote::verify(connection, set).await?;
            if binding.stack.exited_member().is_some() {
                return Err(MutterOutputError::CompositorExited);
            }
            binding.evidence.outputs = verified;
            return Ok(());
        }
        let connection = super::mutter::connect(&binding.stack.bus_address()).await?;
        let observed = super::mutter::current_monitors(&connection).await?;
        if binding.stack.exited_member().is_some() {
            return Err(MutterOutputError::CompositorExited);
        }
        let requests = binding
            .requested
            .iter()
            .map(|placed| placed.request)
            .collect::<Vec<_>>();
        let verified = verify_virtual_outputs(&requests, &observed)?;
        binding.evidence.outputs = verified;
        binding.connection = Some(connection);
        Ok(())
    }

    fn commit(
        &mut self,
        binding: &mut Self::Binding,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        for output in &binding.evidence.outputs {
            tracing::info!(
                target: target::SESSION,
                sid = %binding.session_log_id,
                compositor = binding.evidence.compositor.token(),
                pid = binding.evidence.pid,
                connector = %output.connector,
                width = output.width,
                height = output.height,
                refresh_hz = output.refresh_hz,
                "headless Mutter virtual output committed"
            );
        }
        core::future::ready(Ok(()))
    }

    /// Stops the stack, removing every virtual output with it, and proves
    /// release. Idempotent.
    async fn rollback(&mut self, binding: &mut Self::Binding) -> Result<(), Self::Error> {
        if !binding.armed {
            return Ok(());
        }
        if let Some(mut remote) = binding.remote.take() {
            remote.sessions.stop().await;
        }
        binding.connection.take();
        binding.stack.stop().await?;
        binding.armed = false;
        Ok(())
    }

    fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence {
        &binding.evidence
    }

    fn is_armed(&self, binding: &Self::Binding) -> bool {
        binding.armed
    }
}
