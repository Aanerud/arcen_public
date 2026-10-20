//! The environment contract that hands a compositor capture stream to a
//! capture process.
//!
//! The Linux Pier's headless Wayland session owns the compositor, its private
//! runtime directory and the Mutter `ScreenCast` D-Bus session. It does not
//! read pixels. capenc does, and never talks D-Bus: it receives exactly this
//! hand-off in its process environment. This module is the one definition of
//! the variable names and the one parser both sides use.
//!
//! | Variable | Required | Value |
//! | --- | --- | --- |
//! | `ARCEN_CAPTURE_PROVIDER` | yes | `wayland` |
//! | `XDG_RUNTIME_DIR` | yes | the session's private runtime directory (absolute); holds the `PipeWire` socket |
//! | `PIPEWIRE_REMOTE` | no | `PipeWire`'s own override: a socket name in the runtime directory or an absolute path; `pipewire-0` when absent. The session never sets it |
//! | `ARCEN_PIPEWIRE_NODE` | yes | `PipeWire` node id from `PipeWireStreamAdded` |
//! | `ARCEN_PIPEWIRE_WIDE_NODE` | no | a second stream of the same output whose producer offers wide (ten-bit, half-float) formats: Mutter's `RecordMonitor` of a `RecordVirtual` monitor in an HDR colour state. Only the Grading and HDR compositions consume it |
//! | `ARCEN_CAPTURE_OUTPUT` | yes | compositor connector the stream records, e.g. `Meta-0` |
//! | `ARCEN_CAPTURE_OUTPUT_GENERATION` | yes | output generation the node belongs to (≥ 1) |
//! | `ARCEN_CAPTURE_EXPECTED_SIZE` | yes | `WIDTHxHEIGHT` of the stream's `Parameters` |
//! | `ARCEN_CAPTURE_OUTPUT_POSITION` | yes | `X,Y` logical origin of the output |
//! | `ARCEN_CAPTURE_CURSOR_MODE` | yes | `hidden`, `embedded` or `metadata`, as requested from `ScreenCast` |
//! | `ARCEN_CAPTURE_GPU_PCI` | no | encoder GPU, `domain:bus:device.function` (shared with the Xorg paths); a Wayland session names it only for a proven NVIDIA compositor GPU and no operator `ARCEN_CUDA_DEVICE` |
//! | `ARCEN_COMPOSITOR_GPU_PCI` | no | GPU the compositor renders on, when the session proved it |
//! | `ARCEN_COMPOSITOR_RENDER_NODE` | no | the compositor's DRM render node, `/dev/dri/renderD<minor>` |
//!
//! The pixel format, memory type and modifiers are not part of the hand-off:
//! the compositor's D-Bus API does not state them, so the consumer learns
//! them by `PipeWire` negotiation and reports them as capture-surface
//! evidence. The stream lives as long as the session side keeps its D-Bus
//! connection; a node id is valid for one output generation only.
//!
//! Absent `ARCEN_CAPTURE_PROVIDER` means no hand-off (the Xorg providers).
//! Any other inconsistency is an error, never a guess.

use core::fmt;
use core::num::NonZeroU32;
use std::path::{Path, PathBuf};

use super::PciAddress;
use crate::{OutputIdentity, RegionGeneration};

/// Names the capture provider.
pub const CAPTURE_PROVIDER_ENV: &str = "ARCEN_CAPTURE_PROVIDER";
/// Session runtime directory.
pub const RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";
/// `PipeWire`'s own remote override (read, never written).
pub const PIPEWIRE_REMOTE_ENV: &str = "PIPEWIRE_REMOTE";
/// `PipeWire` node id of the stream to consume.
pub const PIPEWIRE_NODE_ENV: &str = "ARCEN_PIPEWIRE_NODE";
/// `PipeWire` node id of the same output's wide-format stream, when the
/// session records one.
pub const PIPEWIRE_WIDE_NODE_ENV: &str = "ARCEN_PIPEWIRE_WIDE_NODE";
/// Compositor connector the stream records.
pub const CAPTURE_OUTPUT_ENV: &str = "ARCEN_CAPTURE_OUTPUT";
/// Output generation the stream belongs to.
pub const CAPTURE_OUTPUT_GENERATION_ENV: &str = "ARCEN_CAPTURE_OUTPUT_GENERATION";
/// Stream size, `WIDTHxHEIGHT`.
pub const CAPTURE_EXPECTED_SIZE_ENV: &str = "ARCEN_CAPTURE_EXPECTED_SIZE";
/// Output logical origin, `X,Y`.
pub const CAPTURE_OUTPUT_POSITION_ENV: &str = "ARCEN_CAPTURE_OUTPUT_POSITION";
/// Cursor mode requested from the compositor.
pub const CAPTURE_CURSOR_MODE_ENV: &str = "ARCEN_CAPTURE_CURSOR_MODE";
/// Encoder GPU; the Xorg paths set the same variable.
pub const ENCODER_GPU_PCI_ENV: &str = "ARCEN_CAPTURE_GPU_PCI";
/// GPU the compositor renders on.
pub const COMPOSITOR_GPU_PCI_ENV: &str = "ARCEN_COMPOSITOR_GPU_PCI";
/// The compositor's DRM render node.
pub const COMPOSITOR_RENDER_NODE_ENV: &str = "ARCEN_COMPOSITOR_RENDER_NODE";

/// NVIDIA's PCI vendor id: the only vendor whose GPU capenc's native
/// encoder can open.
pub const NVIDIA_PCI_VENDOR: u16 = 0x10de;

/// `PipeWire`'s default socket name.
pub const DEFAULT_PIPEWIRE_SOCKET: &str = "pipewire-0";

/// The `ARCEN_CAPTURE_PROVIDER` value of a Wayland hand-off.
pub const WAYLAND_CAPTURE_PROVIDER: &str = "wayland";

/// Variables only the session's own verified hand-off may set. A session
/// environment must never take them from anywhere else (PAM, the caller).
/// `XDG_RUNTIME_DIR` and `ARCEN_CAPTURE_GPU_PCI` are not listed: other
/// layers legitimately set them too.
pub const CAPTURE_HANDOFF_ENV: &[&str] = &[
    CAPTURE_PROVIDER_ENV,
    PIPEWIRE_REMOTE_ENV,
    PIPEWIRE_NODE_ENV,
    PIPEWIRE_WIDE_NODE_ENV,
    CAPTURE_OUTPUT_ENV,
    CAPTURE_OUTPUT_GENERATION_ENV,
    CAPTURE_EXPECTED_SIZE_ENV,
    CAPTURE_OUTPUT_POSITION_ENV,
    CAPTURE_CURSOR_MODE_ENV,
    COMPOSITOR_GPU_PCI_ENV,
    COMPOSITOR_RENDER_NODE_ENV,
];

const MAX_CONNECTOR_BYTES: usize = 64;

/// How the compositor delivers the cursor (Mutter `ScreenCast` `cursor-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScreenCastCursorMode {
    /// Not drawn and not reported (`0`).
    Hidden,
    /// Composited into the pixels (`1`).
    Embedded,
    /// Reported as `SPA_META_Cursor` metadata (`2`).
    Metadata,
}

impl ScreenCastCursorMode {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Embedded => "embedded",
            Self::Metadata => "metadata",
        }
    }

    /// The epoch's cursor evidence: what the compositor was asked to do
    /// with the cursor for the whole stream. A buffer's own metadata only
    /// says where the cursor is now.
    #[must_use]
    pub const fn cursor_evidence(self) -> super::CursorEvidence {
        match self {
            Self::Hidden => super::CursorEvidence::Absent,
            Self::Embedded => super::CursorEvidence::Composited,
            Self::Metadata => super::CursorEvidence::SeparateMetadata,
        }
    }

    /// Who draws the cursor the client sees for a stream recorded this way:
    /// the host when it is in the pixels, the client otherwise (with the
    /// host's sprite when it is reported as metadata, its own when hidden).
    #[must_use]
    pub const fn cursor_authority(self) -> arcen_protocol::messages::CursorMode {
        match self {
            Self::Embedded => arcen_protocol::messages::CursorMode::Host,
            Self::Hidden | Self::Metadata => arcen_protocol::messages::CursorMode::Local,
        }
    }

    /// Mutter's `cursor-mode` value.
    #[must_use]
    pub const fn mutter_value(self) -> u32 {
        match self {
            Self::Hidden => 0,
            Self::Embedded => 1,
            Self::Metadata => 2,
        }
    }

    /// Parses a token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        [Self::Hidden, Self::Embedded, Self::Metadata]
            .into_iter()
            .find(|mode| mode.token() == token)
    }
}

/// The `cursor-mode` a host records a new desktop's stream with.
///
/// The mode is fixed for the stream's life, so it is chosen when the
/// desktop opens, from the opening client's facts:
///
/// | Preference | Client draws cursor images | Metadata proven on this host | Mode |
/// | --- | --- | --- | --- |
/// | host | any | any | embedded |
/// | local | yes | yes | metadata |
/// | local | no | any | hidden |
/// | local | yes | no | hidden |
///
/// A client that draws the cursor itself never gets one in the pixels as
/// well: its own pointer moves at its display's rate, and a second one
/// composited into the video would trail it at the video's. Without
/// metadata it draws its pointer without the host's shape.
#[must_use]
pub const fn resolve_stream_cursor_mode(
    preference: arcen_protocol::messages::CursorMode,
    client_draws_cursor_images: bool,
    metadata_proven: bool,
) -> ScreenCastCursorMode {
    match preference {
        arcen_protocol::messages::CursorMode::Local
            if client_draws_cursor_images && metadata_proven =>
        {
            ScreenCastCursorMode::Metadata
        }
        arcen_protocol::messages::CursorMode::Local => ScreenCastCursorMode::Hidden,
        arcen_protocol::messages::CursorMode::Host => ScreenCastCursorMode::Embedded,
    }
}

/// One compositor stream handed to a capture process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireCaptureHandoff {
    node_id: NonZeroU32,
    wide_node_id: Option<NonZeroU32>,
    output: OutputIdentity,
    generation: RegionGeneration,
    width: u32,
    height: u32,
    position: (i32, i32),
    cursor_mode: ScreenCastCursorMode,
    runtime_dir: PathBuf,
    pipewire_remote: Option<String>,
    encoder_gpu_pci: Option<PciAddress>,
    compositor_gpu_pci: Option<PciAddress>,
    compositor_render_node: Option<PathBuf>,
}

/// A hand-off that cannot be built or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureHandoffError {
    /// `ARCEN_CAPTURE_PROVIDER` names a provider this contract does not know.
    UnknownProvider(String),
    /// A required variable is absent or empty.
    Missing(&'static str),
    /// A variable's value is malformed.
    Invalid(&'static str),
}

impl fmt::Display for CaptureHandoffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProvider(provider) => {
                write!(formatter, "unknown capture provider {provider:?}")
            }
            Self::Missing(name) => write!(formatter, "capture hand-off is missing {name}"),
            Self::Invalid(name) => write!(formatter, "capture hand-off has an invalid {name}"),
        }
    }
}

impl std::error::Error for CaptureHandoffError {}

fn valid_connector(output: &str) -> bool {
    !output.is_empty()
        && output.len() <= MAX_CONNECTOR_BYTES
        && output
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Parses a PCI address, `domain:bus:device.function` with the domain
/// optional, hexadecimal; the form sysfs and `lspci -D` print.
#[must_use]
pub fn parse_pci_address(value: &str) -> Option<PciAddress> {
    let value = value.trim();
    let (prefix, function) = value.rsplit_once('.')?;
    let mut parts = prefix.rsplit(':');
    let device = parts.next()?;
    let bus = parts.next()?;
    let domain = parts.next().unwrap_or("0");
    if parts.next().is_some() {
        return None;
    }
    let hex = |text: &str, max_len: usize| -> Option<u32> {
        if text.is_empty() || text.len() > max_len {
            return None;
        }
        u32::from_str_radix(text, 16).ok()
    };
    PciAddress::new(
        u16::try_from(hex(domain, 8)?).ok()?,
        u8::try_from(hex(bus, 2)?).ok()?,
        u8::try_from(hex(device, 2)?).ok()?,
        u8::try_from(hex(function, 1)?).ok()?,
    )
    .ok()
}

/// Parses a sysfs PCI `vendor` file (`0x10de`).
#[must_use]
pub fn parse_pci_vendor(text: &str) -> Option<u16> {
    let digits = text.trim().strip_prefix("0x")?;
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    u16::from_str_radix(digits, 16).ok()
}

/// Formats a PCI address the way sysfs names devices (`0000:00:10.0`).
#[must_use]
pub fn format_pci_address(pci: PciAddress) -> String {
    format!(
        "{:04x}:{:02x}:{:02x}.{:x}",
        pci.domain, pci.bus, pci.device, pci.function
    )
}

/// The DRM minor of a `renderD<minor>` node name.
#[must_use]
pub fn render_node_minor_from_name(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("renderD")?;
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Whether `path` is exactly `/dev/dri/renderD<minor>`.
#[must_use]
pub fn is_render_node_path(path: &Path) -> bool {
    path.parent() == Some(Path::new("/dev/dri"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(render_node_minor_from_name)
            .is_some()
}

impl PipeWireCaptureHandoff {
    /// A checked hand-off without GPU facts.
    ///
    /// # Errors
    ///
    /// [`CaptureHandoffError::Invalid`] for node id 0 or `u32::MAX`
    /// (`PipeWire`'s invalid id), generation 0, a zero size, a connector
    /// name that is empty, long or not `[A-Za-z0-9._-]`, or a runtime
    /// directory that is not absolute.
    pub fn new(
        node_id: u32,
        output: &str,
        generation: u64,
        size: (u32, u32),
        position: (i32, i32),
        cursor_mode: ScreenCastCursorMode,
        runtime_dir: &Path,
    ) -> Result<Self, CaptureHandoffError> {
        let node_id = NonZeroU32::new(node_id)
            .filter(|node| node.get() != u32::MAX)
            .ok_or(CaptureHandoffError::Invalid(PIPEWIRE_NODE_ENV))?;
        let generation = RegionGeneration::new(generation)
            .map_err(|_| CaptureHandoffError::Invalid(CAPTURE_OUTPUT_GENERATION_ENV))?;
        if !valid_connector(output) {
            return Err(CaptureHandoffError::Invalid(CAPTURE_OUTPUT_ENV));
        }
        let output = OutputIdentity::new(output)
            .map_err(|_| CaptureHandoffError::Invalid(CAPTURE_OUTPUT_ENV))?;
        if size.0 == 0 || size.1 == 0 {
            return Err(CaptureHandoffError::Invalid(CAPTURE_EXPECTED_SIZE_ENV));
        }
        if !posix_absolute(runtime_dir) {
            return Err(CaptureHandoffError::Invalid(RUNTIME_DIR_ENV));
        }
        Ok(Self {
            node_id,
            wide_node_id: None,
            output,
            generation,
            width: size.0,
            height: size.1,
            position,
            cursor_mode,
            runtime_dir: runtime_dir.to_path_buf(),
            pipewire_remote: None,
            encoder_gpu_pci: None,
            compositor_gpu_pci: None,
            compositor_render_node: None,
        })
    }

    /// Names the output's wide-format stream.
    ///
    /// # Errors
    ///
    /// [`CaptureHandoffError::Invalid`] for node id 0, `u32::MAX`, or the
    /// main stream's own node.
    pub fn with_wide_node(mut self, node_id: Option<u32>) -> Result<Self, CaptureHandoffError> {
        self.wide_node_id = node_id
            .map(|node| {
                NonZeroU32::new(node)
                    .filter(|node| node.get() != u32::MAX && *node != self.node_id)
                    .ok_or(CaptureHandoffError::Invalid(PIPEWIRE_WIDE_NODE_ENV))
            })
            .transpose()?;
        Ok(self)
    }

    /// Names the encoder GPU.
    #[must_use]
    pub const fn with_encoder_gpu(mut self, pci: Option<PciAddress>) -> Self {
        self.encoder_gpu_pci = pci;
        self
    }

    /// Names the compositor's GPU and render node, as far as they are known.
    ///
    /// # Errors
    ///
    /// [`CaptureHandoffError::Invalid`] when the render node is not
    /// `/dev/dri/renderD<minor>`.
    pub fn with_compositor_gpu(
        mut self,
        pci: Option<PciAddress>,
        render_node: Option<&Path>,
    ) -> Result<Self, CaptureHandoffError> {
        if render_node.is_some_and(|node| !is_render_node_path(node)) {
            return Err(CaptureHandoffError::Invalid(COMPOSITOR_RENDER_NODE_ENV));
        }
        self.compositor_gpu_pci = pci;
        self.compositor_render_node = render_node.map(Path::to_path_buf);
        Ok(self)
    }

    /// `PipeWire` node id.
    #[must_use]
    pub const fn node_id(&self) -> u32 {
        self.node_id.get()
    }

    /// `PipeWire` node id of the output's wide-format stream, when recorded.
    #[must_use]
    pub const fn wide_node_id(&self) -> Option<u32> {
        match self.wide_node_id {
            Some(node) => Some(node.get()),
            None => None,
        }
    }

    /// This hand-off pointed at the output's wide-format stream: the node a
    /// Grading or HDR composition consumes. `None` when none is recorded.
    #[must_use]
    pub fn wide(&self) -> Option<Self> {
        let node_id = self.wide_node_id?;
        Some(Self {
            node_id,
            wide_node_id: None,
            ..self.clone()
        })
    }

    /// Compositor connector.
    #[must_use]
    pub const fn output(&self) -> &OutputIdentity {
        &self.output
    }

    /// Output generation.
    #[must_use]
    pub const fn generation(&self) -> RegionGeneration {
        self.generation
    }

    /// Stream size.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Output logical origin.
    #[must_use]
    pub const fn position(&self) -> (i32, i32) {
        self.position
    }

    /// Requested cursor mode.
    #[must_use]
    pub const fn cursor_mode(&self) -> ScreenCastCursorMode {
        self.cursor_mode
    }

    /// Session runtime directory.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// The socket `PipeWire` connects to.
    #[must_use]
    pub fn pipewire_socket(&self) -> PathBuf {
        match self.pipewire_remote.as_deref() {
            Some(remote) if posix_absolute(Path::new(remote)) => PathBuf::from(remote),
            Some(remote) => self.runtime_dir.join(remote),
            None => self.runtime_dir.join(DEFAULT_PIPEWIRE_SOCKET),
        }
    }

    /// Encoder GPU, when named.
    #[must_use]
    pub const fn encoder_gpu_pci(&self) -> Option<PciAddress> {
        self.encoder_gpu_pci
    }

    /// Compositor GPU, when the session proved it.
    #[must_use]
    pub const fn compositor_gpu_pci(&self) -> Option<PciAddress> {
        self.compositor_gpu_pci
    }

    /// Compositor render node, when the session proved it.
    #[must_use]
    pub fn compositor_render_node(&self) -> Option<&Path> {
        self.compositor_render_node.as_deref()
    }

    /// The variables to set on the capture process.
    #[must_use]
    pub fn to_env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            (CAPTURE_PROVIDER_ENV, WAYLAND_CAPTURE_PROVIDER.to_owned()),
            (
                RUNTIME_DIR_ENV,
                self.runtime_dir.to_string_lossy().into_owned(),
            ),
            (PIPEWIRE_NODE_ENV, self.node_id.to_string()),
            (CAPTURE_OUTPUT_ENV, self.output.as_str().to_owned()),
            (
                CAPTURE_OUTPUT_GENERATION_ENV,
                self.generation.get().to_string(),
            ),
            (
                CAPTURE_EXPECTED_SIZE_ENV,
                format!("{}x{}", self.width, self.height),
            ),
            (
                CAPTURE_OUTPUT_POSITION_ENV,
                format!("{},{}", self.position.0, self.position.1),
            ),
            (CAPTURE_CURSOR_MODE_ENV, self.cursor_mode.token().to_owned()),
        ];
        if let Some(node) = self.wide_node_id {
            env.push((PIPEWIRE_WIDE_NODE_ENV, node.to_string()));
        }
        if let Some(pci) = self.encoder_gpu_pci {
            env.push((ENCODER_GPU_PCI_ENV, format_pci_address(pci)));
        }
        if let Some(pci) = self.compositor_gpu_pci {
            env.push((COMPOSITOR_GPU_PCI_ENV, format_pci_address(pci)));
        }
        if let Some(node) = &self.compositor_render_node {
            env.push((
                COMPOSITOR_RENDER_NODE_ENV,
                node.to_string_lossy().into_owned(),
            ));
        }
        env
    }

    /// Reads a hand-off through `lookup` (for example `std::env::var`).
    ///
    /// Returns `Ok(None)` when no provider is named.
    ///
    /// # Errors
    ///
    /// [`CaptureHandoffError`] for an unknown provider or a missing or
    /// malformed variable.
    pub fn from_env<F>(lookup: F) -> Result<Option<Self>, CaptureHandoffError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let Some(provider) = lookup(CAPTURE_PROVIDER_ENV) else {
            return Ok(None);
        };
        if provider != WAYLAND_CAPTURE_PROVIDER {
            return Err(CaptureHandoffError::UnknownProvider(provider));
        }
        let required = |name: &'static str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .ok_or(CaptureHandoffError::Missing(name))
        };
        let node_id = required(PIPEWIRE_NODE_ENV)?
            .parse::<u32>()
            .map_err(|_| CaptureHandoffError::Invalid(PIPEWIRE_NODE_ENV))?;
        let output = required(CAPTURE_OUTPUT_ENV)?;
        let generation = required(CAPTURE_OUTPUT_GENERATION_ENV)?
            .parse::<u64>()
            .map_err(|_| CaptureHandoffError::Invalid(CAPTURE_OUTPUT_GENERATION_ENV))?;
        let size = required(CAPTURE_EXPECTED_SIZE_ENV)?;
        let size = size
            .split_once('x')
            .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
            .ok_or(CaptureHandoffError::Invalid(CAPTURE_EXPECTED_SIZE_ENV))?;
        let position = required(CAPTURE_OUTPUT_POSITION_ENV)?;
        let position = position
            .split_once(',')
            .and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)))
            .ok_or(CaptureHandoffError::Invalid(CAPTURE_OUTPUT_POSITION_ENV))?;
        let cursor_mode = ScreenCastCursorMode::from_token(&required(CAPTURE_CURSOR_MODE_ENV)?)
            .ok_or(CaptureHandoffError::Invalid(CAPTURE_CURSOR_MODE_ENV))?;
        let runtime_dir = PathBuf::from(required(RUNTIME_DIR_ENV)?);
        let pipewire_remote = match lookup(PIPEWIRE_REMOTE_ENV).filter(|value| !value.is_empty()) {
            None => None,
            Some(remote)
                if posix_absolute(Path::new(&remote))
                    || (!remote.contains('/') && remote != "." && remote != "..") =>
            {
                Some(remote)
            }
            Some(_) => return Err(CaptureHandoffError::Invalid(PIPEWIRE_REMOTE_ENV)),
        };
        let pci = |name: &'static str| {
            lookup(name)
                .map(|value| parse_pci_address(&value).ok_or(CaptureHandoffError::Invalid(name)))
                .transpose()
        };
        let encoder_gpu_pci = pci(ENCODER_GPU_PCI_ENV)?;
        let compositor_gpu_pci = pci(COMPOSITOR_GPU_PCI_ENV)?;
        let render_node = lookup(COMPOSITOR_RENDER_NODE_ENV).map(PathBuf::from);
        let wide_node = lookup(PIPEWIRE_WIDE_NODE_ENV)
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| CaptureHandoffError::Invalid(PIPEWIRE_WIDE_NODE_ENV))
            })
            .transpose()?;
        let mut handoff = Self::new(
            node_id,
            &output,
            generation,
            size,
            position,
            cursor_mode,
            &runtime_dir,
        )?
        .with_wide_node(wide_node)?
        .with_encoder_gpu(encoder_gpu_pci)
        .with_compositor_gpu(compositor_gpu_pci, render_node.as_deref())?;
        handoff.pipewire_remote = pipewire_remote;
        Ok(Some(handoff))
    }
}

/// The hand-off names Linux paths: absolute means POSIX-absolute on every
/// host, so the contract parses identically wherever it is compiled.
fn posix_absolute(path: &Path) -> bool {
    path.to_str().is_some_and(|text| text.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    #[test]
    fn stream_cursor_mode_follows_the_opening_client() {
        use arcen_protocol::messages::CursorMode;
        let resolve = super::resolve_stream_cursor_mode;
        assert_eq!(
            resolve(CursorMode::Local, true, true),
            ScreenCastCursorMode::Metadata
        );
        for (images, proven) in [(false, true), (true, false), (false, false)] {
            assert_eq!(
                resolve(CursorMode::Local, images, proven),
                ScreenCastCursorMode::Hidden,
                "a client that draws its own cursor never gets a second one in the pixels"
            );
        }
        for (images, proven) in [(true, true), (false, false)] {
            assert_eq!(
                resolve(CursorMode::Host, images, proven),
                ScreenCastCursorMode::Embedded
            );
        }
        for mode in [
            ScreenCastCursorMode::Hidden,
            ScreenCastCursorMode::Embedded,
            ScreenCastCursorMode::Metadata,
        ] {
            assert_eq!(
                resolve(mode.cursor_authority(), true, true).cursor_authority(),
                mode.cursor_authority()
            );
        }
        assert_eq!(
            ScreenCastCursorMode::Hidden.cursor_authority(),
            CursorMode::Local
        );
    }

    use super::*;

    fn handoff() -> PipeWireCaptureHandoff {
        PipeWireCaptureHandoff::new(
            44,
            "Meta-0",
            1,
            (1920, 1080),
            (0, 0),
            ScreenCastCursorMode::Metadata,
            Path::new("/run/user/1000/arcen-wayland-7"),
        )
        .unwrap()
    }

    fn lookup<'a>(map: &'a BTreeMap<&'static str, String>) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| map.get(name).cloned()
    }

    fn gpu() -> PciAddress {
        PciAddress::new(0, 0, 0x10, 0).unwrap()
    }

    #[test]
    fn round_trips_through_the_environment() {
        let env = handoff().to_env().into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(env[CAPTURE_PROVIDER_ENV], "wayland");
        assert_eq!(env[RUNTIME_DIR_ENV], "/run/user/1000/arcen-wayland-7");
        assert_eq!(env[PIPEWIRE_NODE_ENV], "44");
        assert_eq!(env[CAPTURE_EXPECTED_SIZE_ENV], "1920x1080");
        assert_eq!(env[CAPTURE_CURSOR_MODE_ENV], "metadata");
        assert!(!env.contains_key(ENCODER_GPU_PCI_ENV));
        assert!(!env.contains_key(PIPEWIRE_REMOTE_ENV));
        let parsed = PipeWireCaptureHandoff::from_env(lookup(&env))
            .unwrap()
            .unwrap();
        assert_eq!(parsed, handoff());
        assert_eq!(
            parsed.pipewire_socket(),
            PathBuf::from("/run/user/1000/arcen-wayland-7/pipewire-0")
        );
    }

    #[test]
    fn a_wide_stream_round_trips_and_is_handed_over_on_its_own() {
        let env = handoff()
            .with_wide_node(Some(51))
            .unwrap()
            .to_env()
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(env[PIPEWIRE_WIDE_NODE_ENV], "51");
        let parsed = PipeWireCaptureHandoff::from_env(lookup(&env))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.node_id(), 44);
        assert_eq!(parsed.wide_node_id(), Some(51));
        let wide = parsed.wide().unwrap();
        assert_eq!(wide.node_id(), 51);
        assert_eq!(wide.wide_node_id(), None);
        assert_eq!(wide.output(), parsed.output());
        assert_eq!(handoff().wide(), None);
        for bad in [0, u32::MAX, 44] {
            assert!(handoff().with_wide_node(Some(bad)).is_err(), "{bad}");
        }
        let mut malformed = env.clone();
        malformed.insert(PIPEWIRE_WIDE_NODE_ENV, "x".to_owned());
        assert_eq!(
            PipeWireCaptureHandoff::from_env(lookup(&malformed)),
            Err(CaptureHandoffError::Invalid(PIPEWIRE_WIDE_NODE_ENV))
        );
    }

    #[test]
    fn gpu_facts_round_trip() {
        let full = handoff()
            .with_encoder_gpu(Some(gpu()))
            .with_compositor_gpu(Some(gpu()), Some(Path::new("/dev/dri/renderD128")))
            .unwrap();
        let env = full.to_env().into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(env[ENCODER_GPU_PCI_ENV], "0000:00:10.0");
        assert_eq!(env[COMPOSITOR_GPU_PCI_ENV], "0000:00:10.0");
        assert_eq!(env[COMPOSITOR_RENDER_NODE_ENV], "/dev/dri/renderD128");
        let parsed = PipeWireCaptureHandoff::from_env(lookup(&env))
            .unwrap()
            .unwrap();
        assert_eq!(parsed, full);
        assert_eq!(parsed.compositor_gpu_pci(), Some(gpu()));
        assert_eq!(
            parsed.compositor_render_node(),
            Some(Path::new("/dev/dri/renderD128"))
        );
        assert!(
            handoff()
                .with_compositor_gpu(None, Some(Path::new("/dev/dri/card0")))
                .is_err()
        );
    }

    #[test]
    fn pipewire_remote_is_honoured_but_confined() {
        let mut env = handoff().to_env().into_iter().collect::<BTreeMap<_, _>>();
        env.insert(PIPEWIRE_REMOTE_ENV, "pipewire-arcen".into());
        assert_eq!(
            PipeWireCaptureHandoff::from_env(lookup(&env))
                .unwrap()
                .unwrap()
                .pipewire_socket(),
            PathBuf::from("/run/user/1000/arcen-wayland-7/pipewire-arcen")
        );
        env.insert(PIPEWIRE_REMOTE_ENV, "/run/arcen/pw.sock".into());
        assert_eq!(
            PipeWireCaptureHandoff::from_env(lookup(&env))
                .unwrap()
                .unwrap()
                .pipewire_socket(),
            PathBuf::from("/run/arcen/pw.sock")
        );
        env.insert(PIPEWIRE_REMOTE_ENV, "../pipewire-0".into());
        assert_eq!(
            PipeWireCaptureHandoff::from_env(lookup(&env)),
            Err(CaptureHandoffError::Invalid(PIPEWIRE_REMOTE_ENV))
        );
    }

    #[test]
    fn absent_provider_is_no_handoff() {
        assert_eq!(PipeWireCaptureHandoff::from_env(|_| None), Ok(None));
    }

    #[test]
    fn inconsistencies_are_errors() {
        let mut env = handoff().to_env().into_iter().collect::<BTreeMap<_, _>>();
        env.insert(CAPTURE_PROVIDER_ENV, "xorg".into());
        assert!(matches!(
            PipeWireCaptureHandoff::from_env(lookup(&env)),
            Err(CaptureHandoffError::UnknownProvider(_))
        ));
        let base = handoff().to_env().into_iter().collect::<BTreeMap<_, _>>();
        for name in [
            RUNTIME_DIR_ENV,
            PIPEWIRE_NODE_ENV,
            CAPTURE_OUTPUT_ENV,
            CAPTURE_OUTPUT_GENERATION_ENV,
            CAPTURE_EXPECTED_SIZE_ENV,
            CAPTURE_OUTPUT_POSITION_ENV,
            CAPTURE_CURSOR_MODE_ENV,
        ] {
            let mut missing = base.clone();
            missing.remove(name);
            assert_eq!(
                PipeWireCaptureHandoff::from_env(lookup(&missing)),
                Err(CaptureHandoffError::Missing(name)),
                "{name}"
            );
        }
        for (name, value) in [
            (PIPEWIRE_NODE_ENV, "0"),
            (PIPEWIRE_NODE_ENV, "x"),
            (PIPEWIRE_NODE_ENV, "-1"),
            (PIPEWIRE_NODE_ENV, "4294967295"),
            (CAPTURE_OUTPUT_GENERATION_ENV, "0"),
            (CAPTURE_EXPECTED_SIZE_ENV, "1920"),
            (CAPTURE_EXPECTED_SIZE_ENV, "0x1080"),
            (CAPTURE_OUTPUT_POSITION_ENV, "1"),
            (CAPTURE_CURSOR_MODE_ENV, "drawn"),
            (CAPTURE_OUTPUT_ENV, "Meta 0"),
            (RUNTIME_DIR_ENV, "relative/dir"),
            (ENCODER_GPU_PCI_ENV, "00:10"),
            (COMPOSITOR_GPU_PCI_ENV, "0000:00:20.0"),
            (COMPOSITOR_RENDER_NODE_ENV, "/dev/dri/card0"),
            (COMPOSITOR_RENDER_NODE_ENV, "/tmp/renderD128"),
        ] {
            let mut bad = base.clone();
            bad.insert(name, value.into());
            assert_eq!(
                PipeWireCaptureHandoff::from_env(lookup(&bad)),
                Err(CaptureHandoffError::Invalid(name)),
                "{name}={value}"
            );
        }
    }

    #[test]
    fn pci_and_render_node_parsing_is_strict() {
        assert_eq!(
            parse_pci_address("0001:41:00.1"),
            PciAddress::new(1, 0x41, 0, 1).ok()
        );
        assert_eq!(
            parse_pci_address("41:1f.7"),
            PciAddress::new(0, 0x41, 0x1f, 7).ok()
        );
        for bad in [
            "",
            "41:00",
            "0:0:0:0.0",
            "41:20.0",
            "41:00.8",
            "zz:00.0",
            "41:000.0",
        ] {
            assert_eq!(parse_pci_address(bad), None, "{bad}");
        }
        assert_eq!(parse_pci_address(&format_pci_address(gpu())), Some(gpu()));
        assert_eq!(parse_pci_vendor("0x10de\n"), Some(NVIDIA_PCI_VENDOR));
        assert_eq!(parse_pci_vendor("0x1234"), Some(0x1234));
        for bad in ["10de", "0x", "0x10de0", "0xzz"] {
            assert_eq!(parse_pci_vendor(bad), None, "{bad}");
        }
        assert_eq!(render_node_minor_from_name("renderD128"), Some(128));
        for bad in ["renderD", "renderDx", "card0", "renderD12345", "renderD-1"] {
            assert_eq!(render_node_minor_from_name(bad), None, "{bad}");
        }
        assert!(is_render_node_path(Path::new("/dev/dri/renderD129")));
        assert!(!is_render_node_path(Path::new("/dev/dri/../renderD129")));
    }

    #[test]
    fn cursor_modes_state_their_cursor_truth() {
        use crate::video::CursorEvidence;
        use arcen_protocol::messages::CursorMode;
        let authority = |mode: ScreenCastCursorMode| mode.cursor_evidence().authority();
        assert_eq!(
            authority(ScreenCastCursorMode::Embedded),
            Some(CursorMode::Host)
        );
        assert_eq!(
            authority(ScreenCastCursorMode::Metadata),
            Some(CursorMode::Local)
        );
        assert_eq!(
            authority(ScreenCastCursorMode::Hidden),
            Some(CursorMode::Local)
        );
        assert_eq!(
            ScreenCastCursorMode::Hidden.cursor_evidence(),
            CursorEvidence::Absent
        );
    }

    #[test]
    fn cursor_modes_match_mutter() {
        assert_eq!(ScreenCastCursorMode::Hidden.mutter_value(), 0);
        assert_eq!(ScreenCastCursorMode::Embedded.mutter_value(), 1);
        assert_eq!(ScreenCastCursorMode::Metadata.mutter_value(), 2);
        assert_eq!(
            ScreenCastCursorMode::from_token("metadata"),
            Some(ScreenCastCursorMode::Metadata)
        );
    }
}
