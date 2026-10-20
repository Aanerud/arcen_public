//! Host qualification record for the headless Wayland provider.
//!
//! Some Wayland facts can only be proven by a real session: that a capture
//! route delivered an encoded frame, that session-bound input injection
//! works, and how fast the compositor delivers frames on the route that will
//! serve Speed. A Pier selects its provider at start, before any session
//! exists, so those facts come from a qualification session the operator
//! ran on this host (`arcen-pier wayland qualify --write`), recorded here.
//!
//! The record states which Pier, compositor, `PipeWire` and GPU driver it
//! was proven with. It is valid only for exactly that stack: a different
//! Pier build (version, revision or executable), compositor (kind or
//! version), `PipeWire` version, GPU driver version or DRM presence makes
//! it stale, and a stale or missing record never selects Wayland. The record
//! is host-level evidence only; every session still proves its own first
//! frame and input target at runtime (`wayland_session`).
//!
//! Grading and HDR are recorded as unavailable unless a qualification
//! proves them; no Wayland Grading or HDR composition exists yet.
//!
//! Pure policy and serde types; the Linux Pier gathers the versions, runs
//! the session, and reads and writes the JSON.

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::desktop_provider::{CapabilityFact, WaylandCapabilityFacts};
use crate::wayland_session::HeadlessCompositor;

mod requalify;
mod requested;
mod speed;
mod target;
mod temporary_account;

pub use requalify::{
    AUTO_REQUALIFY_DEADLINE_S, AUTO_REQUALIFY_MAX_INTERRUPTED, AUTO_REQUALIFY_MAX_OUTPUTS,
    AUTO_REQUALIFY_MEASURE_SECONDS, AutoRequalify, REQUALIFY_MARKER_SCHEMA_VERSION,
    REQUALIFY_RETRY_AFTER_S, RequalifyAttempt, auto_requalify_decision, identity_digest,
};
pub use requested::{
    MAX_REQUESTED_OUTPUTS, REQUESTED_SCHEMA_VERSION, RequestedSpeedOutput, RequestedSpeedOutputs,
};
pub use speed::{
    MAX_QUALIFY_OUTPUTS, QUALIFY_CODECS, QualifyOutput, REQUESTED_RESERVED_SLOTS, RateEvidence,
    SPEED_QUALIFICATION_LADDER, SpeedDecision, SpeedRequest, outputs_worth_remeasuring,
    qualification_outputs, resolve_speed,
};
pub use target::{PRODUCTION_SCOPE_UNIT, QualificationTarget, QualificationTargetError};
pub use temporary_account::{DEFAULT_LOGIN_UID_RANGE, login_uid_range, temporary_account_uid};

/// Record schema this build reads and writes. Schema 2 added the DRM
/// module's `modeset` state to the GPU driver identity; schema 3 gives every
/// Speed measurement its Deck display refresh, codec, capture route and
/// evidence kind, and opens [`QualifiedFacts`] to per-package sections.
pub const SCHEMA_VERSION: u32 = 3;

/// The command an operator runs to (re)qualify a host.
pub const QUALIFY_COMMAND: &str = "arcen-pier wayland qualify --write";

/// Where the record lives: the Pier's root-owned state directory.
pub const RECORD_PATH: &str = "/var/lib/arcen/wayland-qualification.json";

/// The temporary local account a default qualification runs as.
pub const QUALIFY_TEMPORARY_USER: &str = "arcen-wlq";

/// Its `GECOS` field: how a leftover from an interrupted run is recognised
/// (and the only `arcen-wlq` the Pier or the uninstaller ever removes).
pub const QUALIFY_TEMPORARY_USER_GECOS: &str = "Arcen Wayland qualification (temporary)";

/// The Pier build a record was proven with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PierBuild {
    /// Package version.
    pub version: String,
    /// Source revision the build was made from, or `unrecorded`.
    pub revision: String,
    /// Lowercase hex SHA-256 of the Pier executable (the fused binary also
    /// carries capenc and the session agent).
    pub executable_sha256: String,
}

/// The GPU kernel driver the encoder runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuDriver {
    /// Driver family, for example `nvidia`.
    pub vendor: String,
    /// Loaded driver version.
    pub version: String,
    /// Whether the driver's DRM/KMS module is loaded (needed for a
    /// compositor to render on, and export buffers from, this GPU).
    pub drm: bool,
    /// The DRM module's kernel mode setting (`modeset`), when it reports
    /// one. It decides whether a compositor gets a GBM/KMS device on this
    /// GPU, which changes its renderer, its buffers and its frame rate.
    pub modeset: Option<bool>,
}

impl Display for GpuDriver {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} {} (drm {}, modeset {})",
            self.vendor,
            self.version,
            if self.drm { "loaded" } else { "absent" },
            match self.modeset {
                Some(true) => "on",
                Some(false) => "off",
                None => "unknown",
            }
        )
    }
}

/// The compositor stack a record was proven on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackVersions {
    /// Compositor the session runs.
    pub compositor: HeadlessCompositor,
    /// That compositor's version, as it reports it.
    pub compositor_version: String,
    /// The side-by-side prefix the compositor runs from; absent for the
    /// distribution's `/usr` (see
    /// [`crate::compositor_prefix::CompositorPrefix::recorded`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compositor_prefix: Option<String>,
    /// `PipeWire` library version.
    pub pipewire_version: String,
    /// Encoder GPU driver, when one is loaded.
    pub gpu_driver: Option<GpuDriver>,
}

/// Everything a record's validity depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    /// Pier build.
    pub pier: PierBuild,
    /// Compositor stack.
    pub stack: StackVersions,
}

/// Operating system, recorded for operators; not a validity rule (a
/// distribution update that matters changes the compositor, `PipeWire` or
/// driver version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OsRelease {
    /// `os-release` `ID`.
    pub id: String,
    /// `os-release` `VERSION_ID`.
    pub version_id: String,
    /// Running kernel release.
    pub kernel: String,
}

/// How the capture consumer moved the compositor's frames into the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureRoute {
    /// Host memory copied into the encoder's input.
    HostCopy,
    /// A same-GPU import followed by a GPU copy or conversion.
    GpuConvert,
    /// A same-GPU import the encoder reads directly.
    ZeroCopyImport,
}

impl CaptureRoute {
    /// Stable token, the `CopyPath` token of the same route.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::HostCopy => "host-copy",
            Self::GpuConvert => "gpu-convert",
            Self::ZeroCopyImport => "zero-copy-import",
        }
    }

    /// Whether the route imports compositor buffers on the GPU.
    #[must_use]
    pub const fn gpu_import(self) -> bool {
        matches!(self, Self::GpuConvert | Self::ZeroCopyImport)
    }
}

/// The qualification session's first encoded frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRouteProof {
    /// Route the frame took into the encoder.
    pub route: CaptureRoute,
    /// Negotiated pixel format token.
    pub format: String,
    /// Buffer memory token.
    pub memory: String,
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Access units the encoder produced on this route during the run.
    pub encoded_access_units: u32,
    /// GPU-imported frames whose pixels matched a host-copy reference of
    /// the same desktop (capenc `wayland-capture-probe verify-gpu-import`).
    /// A GPU route proves GPU import only with at least one. Absent in
    /// records written before the check existed, which read as zero.
    #[serde(default)]
    pub gpu_import_verified_frames: u32,
}

/// What the qualification proved about the host's encoders (a schema-3
/// section, owned by the Wayland software-encode work). Like every
/// optional section it ignores fields it does not know, so a newer build's
/// record is stale for an older one, never malformed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderProof {
    /// Whether a native (hardware) encoder opened for the session's stream
    /// (capenc's `probe-v1`). `missing` makes every Wayland session on the
    /// host a software session, its output fitted to that encoder before the
    /// session exists.
    pub native: CapabilityFact,
    /// The encoder that produced the capture route's access units, as its
    /// `READY` backend token (`native-nvenc`, `openh264-sw-h264`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_route_encoder: Option<String>,
}

/// What session-bound input injection proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputProof {
    /// Injection backend token.
    pub backend: String,
    /// Keyboard.
    pub keyboard: CapabilityFact,
    /// Pointer.
    pub pointer: CapabilityFact,
    /// Touch.
    pub touch: CapabilityFact,
    /// Pen or tablet.
    pub pen: CapabilityFact,
    /// libei (`ConnectToEIS`) injection.
    pub eis: CapabilityFact,
    /// Pointer motions the session accepted during the run.
    pub accepted_pointer_motions: u64,
}

impl InputProof {
    /// Whether keyboard and pointer injection are both proven.
    #[must_use]
    pub fn injection(&self) -> CapabilityFact {
        if self.keyboard == CapabilityFact::Available
            && self.pointer == CapabilityFact::Available
            && self.accepted_pointer_motions > 0
        {
            CapabilityFact::Available
        } else {
            CapabilityFact::Missing
        }
    }
}

/// A sustained source rate measured on one output geometry, refresh, codec
/// and capture route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRateMeasurement {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// The Deck display refresh the measurement was taken for (the output
    /// refresh is decided from it).
    pub display_refresh_hz: u32,
    /// Output refresh rate the compositor was asked for.
    pub refresh_hz: u32,
    /// Encoder codec token the frames were encoded with (`h264`, `h265`,
    /// `av1`): the encoder caps the rate, and differently per codec.
    pub codec: String,
    /// The capture route the frames took.
    pub route: CaptureRoute,
    /// Best rate the source held over consecutive delivering seconds, or
    /// `None` when it never sustained one. A second counts only what was
    /// both delivered and encoded, so an encoder that cannot keep up with
    /// the source caps the rate.
    pub sustained_source_fps: Option<u32>,
    /// Length of the moving phase, in seconds.
    pub seconds: u32,
    /// `measured` for everything a qualification writes; an `inferred`
    /// entry is never admission evidence.
    #[serde(default)]
    pub evidence: RateEvidence,
}

/// What the qualification session proved.
///
/// **Extension convention (schema 3).** A package that proves more facts
/// adds one section of its own here, as
/// `#[serde(default, skip_serializing_if = "Option::is_none")] pub <name>:
/// Option<<Section>>`, owned and documented by that package (multi-output
/// proofs, ten-bit and HDR proofs). A section's absence means "not proven".
/// A section this build does not know is ignored, never an error: it proves
/// nothing to this build and satisfies no capability, and refusing it would
/// make a record written by a newer build unreadable — malformed rather than
/// stale — so the Pier could not re-qualify itself after a downgrade. The
/// record's identity still makes such a record stale for this build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualifiedFacts {
    /// The capture route, when an encoded frame proved it.
    pub capture_route: Option<CaptureRouteProof>,
    /// Session-bound input, when the injector proved it.
    pub input: Option<InputProof>,
    /// Source rates for Speed, per output geometry, refresh, codec and
    /// route.
    pub source_rates: Vec<SourceRateMeasurement>,
    /// A `PulseAudio` monitor source reachable from the session.
    pub audio_monitor: CapabilityFact,
    /// Ten-bit SDR (Grading) capture.
    pub grading: CapabilityFact,
    /// HDR capture.
    pub hdr: CapabilityFact,
    /// X11 applications through the compositor's Xwayland
    /// ([`crate::wayland_x11`]). Absent when qualification did not try it
    /// (the operator turned it off, or Xwayland is not installed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x11_apps: Option<crate::wayland_x11::XwaylandProof>,
    /// Several outputs captured and encoded at once (multi-monitor). Absent
    /// in records that never measured it, which prove one output only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_output: Option<MultiOutputFacts>,
    /// The host's encoders ([`EncoderProof`]). Absent (not proven) reads as
    /// a native encoder unknown: it is tried first, and nothing is fitted to
    /// the software encoder on its account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<EncoderProof>,
    /// How Grading and HDR were proven, or why they were not. Absent in a
    /// record written before the fidelity probe existed: then both stay
    /// missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity: Option<FidelityProof>,
    /// Cursor metadata (`SPA_META_Cursor`) carrying a sprite bitmap on a
    /// `metadata` cursor-mode stream, which lets the Deck draw the cursor.
    /// Absent in records that never proved it, which read as unknown and
    /// keep the compositor's cursor in the video.
    #[serde(default, skip_serializing_if = "is_unknown_fact")]
    pub cursor_metadata: CapabilityFact,
    /// A desktop on its own logind seat with a real pen
    /// ([`crate::native_seat`]), or why not. Absent in records that never
    /// tried it: `wayland.session_mode = auto` then stays headless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_seat: Option<crate::native_seat::NativeSeatProof>,
}

// serde's `skip_serializing_if` passes a reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_unknown_fact(fact: &CapabilityFact) -> bool {
    *fact == CapabilityFact::Unknown
}

/// A drawn ten-bit ramp read back through the real wide capture: whether
/// the compositor composes and screencasts at the precision it claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RampProof {
    /// Negotiated format token (`xbgr2101010`, `rgba-f16`, ...).
    pub format: String,
    /// Memory the frame arrived in (`dmabuf`, `host-shared`).
    pub memory: String,
    /// Distinct codes the ramp was drawn with.
    pub expected_codes: u32,
    /// Distinct codes read back.
    pub distinct_codes: u32,
    /// Largest difference from the drawn code, in codes.
    pub max_error_codes: u32,
    /// The shared rule found natively wide samples.
    pub native_wide: bool,
    /// The patch of three distinct channel codes came back in order (a grey
    /// ramp cannot show a red/blue swap). Absent in a proof written before
    /// the check: then nothing is proven.
    #[serde(default)]
    pub channels_verified: bool,
}

impl RampProof {
    /// Whether this ramp proves its format in its memory.
    #[must_use]
    pub const fn proves(&self) -> bool {
        self.native_wide && self.channels_verified
    }
}

/// What the qualification proved about a wide composition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FidelityProof {
    /// The route to a wide composition (`colour-mode`, `force-hdr`), when
    /// one exists on this stack.
    pub route: Option<String>,
    /// Why Grading and HDR are not available, as a stable token, when they
    /// are not.
    pub gap: Option<String>,
    /// The operator-facing explanation of `gap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Ramp proofs, one per format and memory measured.
    pub ramps: Vec<RampProof>,
    /// The composition's SDR white, measured, in nits.
    pub sdr_white_nits: Option<u16>,
}

impl FidelityProof {
    /// Whether `format` (`xbgr2101010`, `rgba-f16`, ...) in `memory`
    /// (`dmabuf`, `host-shared`) proved natively wide samples in channel
    /// order. A proof is per format and memory: neither vouches for the
    /// other.
    #[must_use]
    pub fn proves(&self, format: &str, memory: &str) -> bool {
        self.proven()
            .any(|ramp| ramp.format == format && ramp.memory == memory)
    }

    /// The ramps that prove their format in their memory.
    pub fn proven(&self) -> impl Iterator<Item = &RampProof> {
        self.ramps.iter().filter(|ramp| ramp.proves())
    }
}

/// One output of a concurrent multi-output run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRateMeasurement {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// The output's own refresh rate (what the compositor paced it at).
    pub refresh_hz: u32,
    /// Best rate the output's source held, every output running at once.
    pub sustained_source_fps: Option<u32>,
    /// Best rate its encoder held, every output running at once.
    pub sustained_encoded_fps: Option<u32>,
}

/// One run with several outputs captured and encoded concurrently, each by
/// its own encoder session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiOutputRun {
    /// Every output of the run, in placement order.
    pub outputs: Vec<OutputRateMeasurement>,
    /// Route every output's frames took into its encoder.
    pub route: CaptureRoute,
    /// Codec token every output encoded with.
    pub codec: String,
    /// Length of the measuring window, in seconds.
    pub seconds: u32,
}

impl MultiOutputRun {
    /// Whether every output encoded frames throughout the run.
    #[must_use]
    pub fn every_output_encoded(&self) -> bool {
        !self.outputs.is_empty()
            && self
                .outputs
                .iter()
                .all(|output| output.sustained_encoded_fps.is_some_and(|fps| fps > 0))
    }
}

/// What concurrent multi-output runs proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiOutputFacts {
    /// The runs, smallest output count first.
    pub runs: Vec<MultiOutputRun>,
}

impl MultiOutputFacts {
    /// Most outputs one run captured and encoded concurrently, every output
    /// encoding throughout; 1 when no run did.
    #[must_use]
    pub fn max_proven_outputs(&self) -> usize {
        self.runs
            .iter()
            .filter(|run| run.every_output_encoded())
            .map(|run| run.outputs.len())
            .max()
            .unwrap_or(1)
            .max(1)
    }
}

/// The versioned host qualification record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaylandQualificationRecord {
    /// [`SCHEMA_VERSION`] when written.
    pub schema_version: u32,
    /// When the qualification finished, seconds since the Unix epoch.
    pub qualified_at_unix_s: u64,
    /// What the record's validity depends on.
    pub host: HostIdentity,
    /// Operating system, informational.
    pub os: OsRelease,
    /// What the session proved.
    pub facts: QualifiedFacts,
}

/// Just the schema version, read before the full record so a record of
/// another schema is reported as such rather than as malformed.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct SchemaProbe {
    /// Schema the record claims.
    pub schema_version: u32,
}

/// Why a record no longer describes this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReason {
    /// Another Pier version.
    PierVersion {
        /// Recorded.
        recorded: String,
        /// Running.
        current: String,
    },
    /// Another source revision.
    PierRevision {
        /// Recorded.
        recorded: String,
        /// Running.
        current: String,
    },
    /// Another Pier executable.
    PierExecutable,
    /// Another compositor.
    Compositor {
        /// Recorded.
        recorded: HeadlessCompositor,
        /// Installed.
        current: HeadlessCompositor,
    },
    /// Another compositor version.
    CompositorVersion {
        /// Recorded.
        recorded: String,
        /// Installed.
        current: String,
    },
    /// The compositor runs from another prefix (`None` is `/usr`).
    CompositorPrefix {
        /// Recorded.
        recorded: Option<String>,
        /// Configured.
        current: Option<String>,
    },
    /// Another `PipeWire` version.
    PipeWireVersion {
        /// Recorded.
        recorded: String,
        /// Installed.
        current: String,
    },
    /// Another GPU driver, or its DRM module appeared, went away or changed
    /// its mode setting.
    GpuDriver {
        /// Recorded.
        recorded: Option<GpuDriver>,
        /// Loaded.
        current: Option<GpuDriver>,
    },
}

impl StaleReason {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::PierVersion { .. } => "pier_version",
            Self::PierRevision { .. } => "pier_revision",
            Self::PierExecutable => "pier_executable",
            Self::Compositor { .. } => "compositor",
            Self::CompositorVersion { .. } => "compositor_version",
            Self::CompositorPrefix { .. } => "compositor_prefix",
            Self::PipeWireVersion { .. } => "pipewire_version",
            Self::GpuDriver { .. } => "gpu_driver",
        }
    }
}

fn driver_text(driver: Option<&GpuDriver>) -> String {
    driver.map_or_else(|| "none".to_owned(), ToString::to_string)
}

impl Display for StaleReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PierVersion { recorded, current } => {
                write!(formatter, "Pier version {recorded} -> {current}")
            }
            Self::PierRevision { recorded, current } => {
                write!(formatter, "Pier revision {recorded} -> {current}")
            }
            Self::PierExecutable => formatter.write_str("the Pier executable changed"),
            Self::Compositor { recorded, current } => write!(
                formatter,
                "compositor {} -> {}",
                recorded.token(),
                current.token()
            ),
            Self::CompositorVersion { recorded, current } => {
                write!(formatter, "compositor version {recorded} -> {current}")
            }
            Self::CompositorPrefix { recorded, current } => write!(
                formatter,
                "compositor prefix {} -> {}",
                recorded
                    .as_deref()
                    .unwrap_or(crate::compositor_prefix::SYSTEM_PREFIX),
                current
                    .as_deref()
                    .unwrap_or(crate::compositor_prefix::SYSTEM_PREFIX)
            ),
            Self::PipeWireVersion { recorded, current } => {
                write!(formatter, "PipeWire {recorded} -> {current}")
            }
            Self::GpuDriver { recorded, current } => write!(
                formatter,
                "GPU driver {} -> {}",
                driver_text(recorded.as_ref()),
                driver_text(current.as_ref())
            ),
        }
    }
}

/// Why no valid record is available. Every variant tells the operator what
/// to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualificationError {
    /// No record at the path.
    Missing {
        /// Record path.
        path: String,
    },
    /// The record could not be read, or is not a root-owned file only root
    /// can change.
    Unreadable {
        /// Record path.
        path: String,
        /// What failed.
        reason: String,
    },
    /// The record is not valid JSON of this schema.
    Malformed {
        /// What failed.
        reason: String,
    },
    /// The record was written for another schema.
    UnsupportedSchema {
        /// Schema found.
        found: u32,
    },
    /// The host changed since qualification.
    Stale(Vec<StaleReason>),
}

impl QualificationError {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "missing",
            Self::Unreadable { .. } => "unreadable",
            Self::Malformed { .. } => "malformed",
            Self::UnsupportedSchema { .. } => "unsupported_schema",
            Self::Stale(_) => "stale",
        }
    }
}

impl Display for QualificationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { path } => write!(
                formatter,
                "this host has no Wayland qualification record ({path})"
            )?,
            Self::Unreadable { path, reason } => write!(
                formatter,
                "the Wayland qualification record {path} cannot be used: {reason}"
            )?,
            Self::Malformed { reason } => write!(
                formatter,
                "the Wayland qualification record is malformed: {reason}"
            )?,
            Self::UnsupportedSchema { found } => write!(
                formatter,
                "the Wayland qualification record has schema {found}; this Pier reads schema \
                 {SCHEMA_VERSION}"
            )?,
            Self::Stale(reasons) => {
                formatter.write_str("the Wayland qualification record is stale (")?;
                for (index, reason) in reasons.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{reason}")?;
                }
                formatter.write_str(")")?;
            }
        }
        write!(formatter, "; run `{QUALIFY_COMMAND}` as root")
    }
}

impl std::error::Error for QualificationError {}

/// Refuses a schema this build does not read.
///
/// # Errors
///
/// [`QualificationError::UnsupportedSchema`].
pub const fn check_schema(probe: SchemaProbe) -> Result<(), QualificationError> {
    if probe.schema_version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(QualificationError::UnsupportedSchema {
            found: probe.schema_version,
        })
    }
}

/// Every way `current` differs from what the record was proven with.
#[must_use]
pub fn stale_reasons(recorded: &HostIdentity, current: &HostIdentity) -> Vec<StaleReason> {
    let mut reasons = Vec::new();
    if recorded.pier.version != current.pier.version {
        reasons.push(StaleReason::PierVersion {
            recorded: recorded.pier.version.clone(),
            current: current.pier.version.clone(),
        });
    }
    if recorded.pier.revision != current.pier.revision {
        reasons.push(StaleReason::PierRevision {
            recorded: recorded.pier.revision.clone(),
            current: current.pier.revision.clone(),
        });
    }
    if !recorded
        .pier
        .executable_sha256
        .eq_ignore_ascii_case(&current.pier.executable_sha256)
    {
        reasons.push(StaleReason::PierExecutable);
    }
    if recorded.stack.compositor != current.stack.compositor {
        reasons.push(StaleReason::Compositor {
            recorded: recorded.stack.compositor,
            current: current.stack.compositor,
        });
    }
    if recorded.stack.compositor_version != current.stack.compositor_version {
        reasons.push(StaleReason::CompositorVersion {
            recorded: recorded.stack.compositor_version.clone(),
            current: current.stack.compositor_version.clone(),
        });
    }
    if recorded.stack.compositor_prefix != current.stack.compositor_prefix {
        reasons.push(StaleReason::CompositorPrefix {
            recorded: recorded.stack.compositor_prefix.clone(),
            current: current.stack.compositor_prefix.clone(),
        });
    }
    if recorded.stack.pipewire_version != current.stack.pipewire_version {
        reasons.push(StaleReason::PipeWireVersion {
            recorded: recorded.stack.pipewire_version.clone(),
            current: current.stack.pipewire_version.clone(),
        });
    }
    if recorded.stack.gpu_driver != current.stack.gpu_driver {
        reasons.push(StaleReason::GpuDriver {
            recorded: recorded.stack.gpu_driver.clone(),
            current: current.stack.gpu_driver.clone(),
        });
    }
    reasons
}

impl WaylandQualificationRecord {
    /// The facts this record proves. A host copy proves the capture route
    /// but never GPU import; a record without a route or input proof leaves
    /// that fact missing.
    #[must_use]
    pub fn proven_capture_route(&self) -> CapabilityFact {
        match &self.facts.capture_route {
            Some(proof) if proof.encoded_access_units > 0 => CapabilityFact::Available,
            _ => CapabilityFact::Missing,
        }
    }

    /// Whether the record proves GPU import: a GPU route that encoded
    /// frames and whose imported pixels were verified against a host copy.
    #[must_use]
    pub fn proven_gpu_import(&self) -> CapabilityFact {
        match &self.facts.capture_route {
            Some(proof)
                if proof.route.gpu_import()
                    && proof.encoded_access_units > 0
                    && proof.gpu_import_verified_frames > 0 =>
            {
                CapabilityFact::Available
            }
            _ => CapabilityFact::Unknown,
        }
    }

    /// The wide-composition proof, when the record carries one.
    #[must_use]
    pub const fn fidelity(&self) -> Option<&FidelityProof> {
        self.facts.fidelity.as_ref()
    }

    /// Whether the record proves session-bound input injection.
    #[must_use]
    pub fn proven_input_injection(&self) -> CapabilityFact {
        self.facts
            .input
            .as_ref()
            .map_or(CapabilityFact::Missing, InputProof::injection)
    }

    /// Whether the record proves cursor metadata with a sprite bitmap.
    #[must_use]
    pub const fn proven_cursor_metadata(&self) -> CapabilityFact {
        self.facts.cursor_metadata
    }

    /// Whether a native (hardware) encoder opened for the qualification
    /// session's stream.
    #[must_use]
    pub fn proven_native_encoder(&self) -> CapabilityFact {
        self.facts
            .encoder
            .as_ref()
            .map_or(CapabilityFact::Unknown, |encoder| encoder.native)
    }

    /// Checks the record against the running host.
    ///
    /// # Errors
    ///
    /// [`QualificationError::UnsupportedSchema`] or
    /// [`QualificationError::Stale`] naming every difference.
    pub fn validate(self, current: &HostIdentity) -> Result<QualifiedHost, QualificationError> {
        check_schema(SchemaProbe {
            schema_version: self.schema_version,
        })?;
        let reasons = stale_reasons(&self.host, current);
        if reasons.is_empty() {
            Ok(QualifiedHost { record: self })
        } else {
            Err(QualificationError::Stale(reasons))
        }
    }
}

/// A record that describes the running host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedHost {
    record: WaylandQualificationRecord,
}

impl QualifiedHost {
    /// The validated record.
    #[must_use]
    pub const fn record(&self) -> &WaylandQualificationRecord {
        &self.record
    }

    /// The live host facts with the session-proven ones taken from the
    /// record. A fact the live probe proved missing is never hidden.
    #[must_use]
    pub fn apply(&self, live: WaylandCapabilityFacts) -> WaylandCapabilityFacts {
        let take = |live: CapabilityFact, recorded: CapabilityFact| {
            if live == CapabilityFact::Missing {
                CapabilityFact::Missing
            } else {
                recorded
            }
        };
        WaylandCapabilityFacts {
            capture_route: take(live.capture_route, self.record.proven_capture_route()),
            gpu_import: take(live.gpu_import, self.record.proven_gpu_import()),
            input_injection: take(live.input_injection, self.record.proven_input_injection()),
            ..live
        }
    }

    /// How a Speed request is served on this host, from the record's
    /// measurements (see [`resolve_speed`]); `None` serves it as Auto.
    #[must_use]
    pub fn speed_decision(&self, request: &SpeedRequest<'_>) -> Option<SpeedDecision> {
        resolve_speed(&self.record.facts.source_rates, request)
    }

    /// Most outputs this host proved it captures and encodes at once; 1 for
    /// a record without a multi-output run.
    #[must_use]
    pub fn max_concurrent_outputs(&self) -> usize {
        self.record
            .facts
            .multi_output
            .as_ref()
            .map_or(1, MultiOutputFacts::max_proven_outputs)
    }

    /// The native-seat section, when qualification ran it.
    #[must_use]
    pub fn native_seat(&self) -> Option<&crate::native_seat::NativeSeatProof> {
        self.record.facts.native_seat.as_ref()
    }

    /// The capture route a session takes on this host: the GPU route when
    /// the record proves GPU import, otherwise the host copy.
    #[must_use]
    pub fn serving_route(&self) -> CaptureRoute {
        match &self.record.facts.capture_route {
            Some(proof) if self.record.proven_gpu_import() == CapabilityFact::Available => {
                proof.route
            }
            _ => CaptureRoute::HostCopy,
        }
    }
}

/// The slowest sustained source rate that qualifies Speed with a contract
/// ceiling of `ceiling_fps`: 95 % of it, so jitter of a source that holds
/// the rate does not disqualify it.
#[must_use]
pub const fn speed_minimum_fps(ceiling_fps: u32) -> u32 {
    ceiling_fps.saturating_mul(95).div_ceil(100)
}

/// The version `gnome-shell --version` (`GNOME Shell 49.4`) or
/// `mutter --version` (`mutter 49.4`) reports: the first token that starts
/// with a digit.
#[must_use]
pub fn parse_compositor_version(output: &str) -> Option<String> {
    output
        .lines()
        .flat_map(str::split_whitespace)
        .find(|token| token.starts_with(|character: char| character.is_ascii_digit()))
        .map(str::to_owned)
}

/// The library version `pipewire --version` reports (`Linked with
/// libpipewire 1.4.11`, else `Compiled with libpipewire …`).
#[must_use]
pub fn parse_pipewire_version(output: &str) -> Option<String> {
    let version_after = |prefix: &str| {
        output.lines().find_map(|line| {
            line.trim()
                .strip_prefix(prefix)
                .and_then(|rest| rest.split_whitespace().next())
                .map(str::to_owned)
        })
    };
    version_after("Linked with libpipewire").or_else(|| version_after("Compiled with libpipewire"))
}

/// `ID` and `VERSION_ID` from `os-release` text.
#[must_use]
pub fn parse_os_release(text: &str) -> (String, String) {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(|value| value.trim().trim_matches('"').to_owned())
            .unwrap_or_default()
    };
    (value("ID"), value("VERSION_ID"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fidelity_proof_is_per_format_and_memory_and_needs_channel_order() {
        let ramp = |format: &str, memory: &str, channels_verified| RampProof {
            format: format.into(),
            memory: memory.into(),
            expected_codes: 1024,
            distinct_codes: 1024,
            max_error_codes: 0,
            native_wide: true,
            channels_verified,
        };
        let proof = FidelityProof {
            route: Some("force-hdr".into()),
            gap: None,
            detail: None,
            ramps: vec![
                ramp("xbgr2101010", "dmabuf", true),
                ramp("xrgb2101010", "host-shared", false),
            ],
            sdr_white_nits: Some(203),
        };
        assert!(proof.proves("xbgr2101010", "dmabuf"));
        assert!(!proof.proves("rgba-f16", "dmabuf"));
        assert!(!proof.proves("xbgr2101010", "host-shared"));
        // Wide codes with the wrong channel order prove nothing.
        assert!(!proof.proves("xrgb2101010", "host-shared"));
        // A ramp written before the channel check reads as unverified.
        let old: RampProof = serde_json::from_str(
            r#"{"format":"xbgr2101010","memory":"dmabuf","expected_codes":1024,
                "distinct_codes":1024,"max_error_codes":0,"native_wide":true}"#,
        )
        .unwrap();
        assert!(!old.proves());
    }

    fn identity() -> HostIdentity {
        HostIdentity {
            pier: PierBuild {
                version: "0.17.0".into(),
                revision: "0f73bc76".into(),
                executable_sha256: "ab".repeat(32),
            },
            stack: StackVersions {
                compositor: HeadlessCompositor::GnomeShell,
                compositor_version: "49.4".into(),
                compositor_prefix: None,
                pipewire_version: "1.4.11".into(),
                gpu_driver: Some(GpuDriver {
                    vendor: "nvidia".into(),
                    version: "580.65.06".into(),
                    drm: true,
                    modeset: Some(false),
                }),
            },
        }
    }

    fn record() -> WaylandQualificationRecord {
        WaylandQualificationRecord {
            schema_version: SCHEMA_VERSION,
            qualified_at_unix_s: 1_792_000_000,
            host: identity(),
            os: OsRelease {
                id: "rocky".into(),
                version_id: "10.2".into(),
                kernel: "6.12.0".into(),
            },
            facts: QualifiedFacts {
                capture_route: Some(CaptureRouteProof {
                    route: CaptureRoute::HostCopy,
                    format: "bgrx8".into(),
                    memory: "host-shared".into(),
                    width: 1920,
                    height: 1080,
                    encoded_access_units: 90,
                    gpu_import_verified_frames: 0,
                }),
                input: Some(InputProof {
                    backend: "remote_desktop_dbus".into(),
                    keyboard: CapabilityFact::Available,
                    pointer: CapabilityFact::Available,
                    touch: CapabilityFact::Available,
                    pen: CapabilityFact::Missing,
                    eis: CapabilityFact::Missing,
                    accepted_pointer_motions: 120,
                }),
                source_rates: vec![SourceRateMeasurement {
                    width: 1920,
                    height: 1080,
                    display_refresh_hz: 60,
                    refresh_hz: 60,
                    codec: "h265".into(),
                    route: CaptureRoute::HostCopy,
                    sustained_source_fps: Some(58),
                    seconds: 8,
                    evidence: RateEvidence::Measured,
                }],
                audio_monitor: CapabilityFact::Available,
                grading: CapabilityFact::Missing,
                hdr: CapabilityFact::Missing,
                x11_apps: None,
                cursor_metadata: CapabilityFact::Unknown,
                native_seat: None,
                multi_output: None,
                fidelity: None,
                encoder: Some(EncoderProof {
                    native: CapabilityFact::Available,
                    capture_route_encoder: Some("native-nvenc".into()),
                }),
            },
        }
    }

    #[test]
    fn cursor_metadata_is_optional_and_omitted_until_proven() {
        let unproven = record();
        let json = serde_json::to_value(&unproven).unwrap();
        assert!(json["facts"].get("cursor_metadata").is_none());
        let parsed: WaylandQualificationRecord = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.proven_cursor_metadata(), CapabilityFact::Unknown);

        let mut proven = record();
        proven.facts.cursor_metadata = CapabilityFact::Available;
        let json = serde_json::to_value(&proven).unwrap();
        assert_eq!(json["facts"]["cursor_metadata"], "available");
        let parsed: WaylandQualificationRecord = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.proven_cursor_metadata(), CapabilityFact::Available);
    }

    #[test]
    fn the_encoder_section_is_optional_and_a_software_host_still_proves_its_route() {
        // A record without the section (an older build, or never probed)
        // reads as unknown: native is tried first, nothing is fitted.
        let mut json = serde_json::to_value(record()).unwrap();
        json["facts"].as_object_mut().unwrap().remove("encoder");
        let old: WaylandQualificationRecord = serde_json::from_value(json).unwrap();
        assert_eq!(old.proven_native_encoder(), CapabilityFact::Unknown);
        let unknown = serde_json::to_value(&old).unwrap();
        assert!(unknown["facts"].get("encoder").is_none());

        // A newer build's extra field in the section is ignored.
        let mut newer = serde_json::to_value(record()).unwrap();
        newer["facts"]["encoder"]["vaapi"] = serde_json::json!("available");
        let parsed: WaylandQualificationRecord = serde_json::from_value(newer).unwrap();
        assert_eq!(parsed.proven_native_encoder(), CapabilityFact::Available);

        // An Intel/AMD host: no native encoder, the route proven by OpenH264.
        let mut software = record();
        software.facts.encoder = Some(EncoderProof {
            native: CapabilityFact::Missing,
            capture_route_encoder: Some("openh264-sw-h264".into()),
        });
        software.host.stack.gpu_driver = None;
        software.facts.source_rates.clear();
        let json = serde_json::to_value(&software).unwrap();
        assert_eq!(json["facts"]["encoder"]["native"], "missing");
        assert_eq!(
            json["facts"]["encoder"]["capture_route_encoder"],
            "openh264-sw-h264"
        );
        let parsed: WaylandQualificationRecord = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.proven_native_encoder(), CapabilityFact::Missing);
        assert_eq!(parsed.proven_capture_route(), CapabilityFact::Available);
        assert_ne!(parsed.proven_gpu_import(), CapabilityFact::Available);
    }

    const LIVE: WaylandCapabilityFacts = WaylandCapabilityFacts {
        compositor: CapabilityFact::Available,
        session_service: CapabilityFact::Available,
        pipewire: CapabilityFact::Available,
        render_node_access: CapabilityFact::Available,
        capture_route: CapabilityFact::Unknown,
        gpu_import: CapabilityFact::Unknown,
        input_injection: CapabilityFact::Unknown,
    };

    #[test]
    fn concurrent_outputs_are_proven_only_by_a_run_where_every_output_encoded() {
        let host = record().validate(&identity()).unwrap();
        assert_eq!(host.max_concurrent_outputs(), 1);

        let output = |encoded: Option<u32>| OutputRateMeasurement {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            sustained_source_fps: encoded,
            sustained_encoded_fps: encoded,
        };
        let run = |outputs: Vec<OutputRateMeasurement>| MultiOutputRun {
            outputs,
            route: CaptureRoute::HostCopy,
            codec: "h265".into(),
            seconds: 6,
        };
        let mut with_runs = record();
        with_runs.facts.multi_output = Some(MultiOutputFacts {
            runs: vec![
                run(vec![output(Some(59)), output(Some(58))]),
                run(vec![
                    output(Some(30)),
                    output(Some(30)),
                    output(None),
                    output(Some(30)),
                ]),
            ],
        });
        let host = with_runs.clone().validate(&identity()).unwrap();
        assert_eq!(host.max_concurrent_outputs(), 2);

        // Absent in older records; written only when measured.
        let json = serde_json::to_value(record()).unwrap();
        assert!(json["facts"].get("multi_output").is_none());
        let back: WaylandQualificationRecord =
            serde_json::from_value(serde_json::to_value(&with_runs).unwrap()).unwrap();
        assert_eq!(back, with_runs);
    }

    #[test]
    fn a_valid_record_supplies_the_session_proven_facts() {
        use crate::desktop_provider::{
            DesktopProvider, HostDesktopFacts, ProviderPreference, select_desktop_provider,
        };
        let without_record = HostDesktopFacts {
            xorg_server: false,
            wayland_built: true,
            wayland: LIVE,
            qualification: Some(QualificationError::Missing {
                path: "/var/lib/arcen/wayland-qualification.json".into(),
            }),
        };
        assert!(select_desktop_provider(ProviderPreference::Auto, &without_record).is_err());

        let host = record().validate(&identity()).unwrap();
        let facts = host.apply(LIVE);
        assert_eq!(facts.capture_route, CapabilityFact::Available);
        assert_eq!(facts.input_injection, CapabilityFact::Available);
        // A host copy never proves GPU import.
        assert_eq!(facts.gpu_import, CapabilityFact::Unknown);
        assert_eq!(facts.render_node_access, CapabilityFact::Available);
        let selection = select_desktop_provider(
            ProviderPreference::Auto,
            &HostDesktopFacts {
                wayland: facts,
                qualification: None,
                ..without_record
            },
        )
        .unwrap();
        assert_eq!(selection.provider, DesktopProvider::Wayland);
    }

    #[test]
    fn a_record_never_hides_a_fact_the_live_probe_proved_missing() {
        let host = record().validate(&identity()).unwrap();
        let facts = host.apply(WaylandCapabilityFacts {
            input_injection: CapabilityFact::Missing,
            ..LIVE
        });
        assert_eq!(facts.input_injection, CapabilityFact::Missing);
    }

    #[test]
    fn an_incomplete_record_proves_nothing_it_lacks() {
        let mut incomplete = record();
        incomplete
            .facts
            .capture_route
            .as_mut()
            .unwrap()
            .encoded_access_units = 0;
        incomplete
            .facts
            .input
            .as_mut()
            .unwrap()
            .accepted_pointer_motions = 0;
        let facts = incomplete.validate(&identity()).unwrap().apply(LIVE);
        assert_eq!(facts.capture_route, CapabilityFact::Missing);
        assert_eq!(facts.input_injection, CapabilityFact::Missing);

        let mut no_proofs = record();
        no_proofs.facts.capture_route = None;
        no_proofs.facts.input = None;
        let facts = no_proofs.validate(&identity()).unwrap().apply(LIVE);
        assert_eq!(facts.capture_route, CapabilityFact::Missing);
        assert_eq!(facts.input_injection, CapabilityFact::Missing);
    }

    #[test]
    fn a_verified_gpu_route_proves_gpu_import() {
        let mut gpu = record();
        gpu.facts.capture_route.as_mut().unwrap().route = CaptureRoute::GpuConvert;
        // Encoded but never compared with a host copy: not proven.
        assert_eq!(gpu.proven_gpu_import(), CapabilityFact::Unknown);
        gpu.facts
            .capture_route
            .as_mut()
            .unwrap()
            .gpu_import_verified_frames = 1;
        assert_eq!(gpu.proven_gpu_import(), CapabilityFact::Available);
        // A host copy never proves GPU import, verified or not.
        let mut host = gpu.clone();
        host.facts.capture_route.as_mut().unwrap().route = CaptureRoute::HostCopy;
        assert_eq!(host.proven_gpu_import(), CapabilityFact::Unknown);
        // A record written before the field existed reads as unverified.
        let mut value = serde_json::to_value(&gpu).unwrap();
        value["facts"]["capture_route"]
            .as_object_mut()
            .unwrap()
            .remove("gpu_import_verified_frames");
        let older: WaylandQualificationRecord = serde_json::from_value(value).unwrap();
        assert_eq!(older.proven_gpu_import(), CapabilityFact::Unknown);
    }

    #[test]
    fn every_stack_change_makes_the_record_stale_and_names_it() {
        type Change = fn(&mut HostIdentity);
        let cases: Vec<(Change, &str)> = vec![
            (|host| host.pier.version = "0.17.1".into(), "pier_version"),
            (
                |host| host.pier.revision = "1234abcd".into(),
                "pier_revision",
            ),
            (
                |host| host.pier.executable_sha256 = "cd".repeat(32),
                "pier_executable",
            ),
            (
                |host| host.stack.compositor = HeadlessCompositor::Mutter,
                "compositor",
            ),
            (
                |host| host.stack.compositor_version = "49.5".into(),
                "compositor_version",
            ),
            (
                |host| host.stack.compositor_prefix = Some("/opt/arcen-gnome/51".into()),
                "compositor_prefix",
            ),
            (
                |host| host.stack.pipewire_version = "1.4.12".into(),
                "pipewire_version",
            ),
            (
                |host| host.stack.gpu_driver.as_mut().unwrap().version = "580.105.08".into(),
                "gpu_driver",
            ),
            (
                |host| host.stack.gpu_driver.as_mut().unwrap().drm = false,
                "gpu_driver",
            ),
            (
                |host| host.stack.gpu_driver.as_mut().unwrap().modeset = Some(true),
                "gpu_driver",
            ),
            (
                |host| host.stack.gpu_driver.as_mut().unwrap().modeset = None,
                "gpu_driver",
            ),
            (|host| host.stack.gpu_driver = None, "gpu_driver"),
        ];
        for (change, token) in cases {
            let mut current = identity();
            change(&mut current);
            let error = record().validate(&current).unwrap_err();
            let QualificationError::Stale(reasons) = &error else {
                panic!("{error:?}");
            };
            assert_eq!(reasons.len(), 1, "{token}");
            assert_eq!(reasons[0].token(), token);
            let message = error.to_string();
            assert!(message.contains("stale"), "{message}");
            assert!(message.contains(QUALIFY_COMMAND), "{message}");
        }
        // The digest compares without regard to hex case.
        let mut upper = identity();
        upper.pier.executable_sha256 = upper.pier.executable_sha256.to_uppercase();
        assert!(record().validate(&upper).is_ok());
    }

    #[test]
    fn a_record_proven_on_usr_names_no_prefix_and_a_prefix_change_is_explained() {
        let host = identity();
        let json = serde_json::to_string(&host).unwrap();
        assert!(!json.contains("compositor_prefix"), "{json}");
        let parsed: HostIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stack.compositor_prefix, None);
        let mut moved = identity();
        moved.stack.compositor_prefix = Some("/opt/arcen-gnome/51".into());
        let reasons = stale_reasons(&host, &moved);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0].to_string(),
            "compositor prefix /usr -> /opt/arcen-gnome/51"
        );
    }

    #[test]
    fn every_refusal_names_the_qualification_command() {
        for error in [
            QualificationError::Missing {
                path: "/var/lib/arcen/wayland-qualification.json".into(),
            },
            QualificationError::Unreadable {
                path: "/var/lib/arcen/wayland-qualification.json".into(),
                reason: "not owned by root".into(),
            },
            QualificationError::Malformed {
                reason: "missing field".into(),
            },
            QualificationError::UnsupportedSchema { found: 2 },
        ] {
            assert!(error.to_string().contains(QUALIFY_COMMAND), "{error}");
        }
    }

    #[test]
    fn the_record_round_trips_and_refuses_unknown_fields_and_schemas() {
        let json = serde_json::to_string_pretty(&record()).unwrap();
        assert!(json.contains("\"compositor\": \"gnome-shell\""));
        assert!(json.contains("\"route\": \"host-copy\""));
        assert!(json.contains("\"grading\": \"missing\""));
        let parsed: WaylandQualificationRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, record());

        // An unknown top-level field is refused; an unknown facts section
        // (another package's proof) is ignored and proves nothing here.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["surprise"] = serde_json::json!(true);
        assert!(serde_json::from_value::<WaylandQualificationRecord>(value).is_err());
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["facts"]["future_proof"] = serde_json::json!({"outputs_max_proven": 2, "runs": []});
        let newer: WaylandQualificationRecord = serde_json::from_value(value).unwrap();
        assert_eq!(newer.facts, record().facts);

        let future: SchemaProbe =
            serde_json::from_str(r#"{"schema_version":4,"anything":{}}"#).unwrap();
        assert_eq!(
            check_schema(future),
            Err(QualificationError::UnsupportedSchema { found: 4 })
        );
        // Schema 2 (no codec, route or display refresh per rate) is refused.
        let mut two = record();
        two.schema_version = 2;
        assert!(matches!(
            two.validate(&identity()),
            Err(QualificationError::UnsupportedSchema { found: 2 })
        ));
        assert!(json.contains("\"codec\": \"h265\""));
        assert!(json.contains("\"kind\": \"measured\""));
        // A schema-1 record (no modeset) is refused as such, not trusted.
        let mut other = record();
        other.schema_version = 1;
        assert!(matches!(
            other.validate(&identity()),
            Err(QualificationError::UnsupportedSchema { found: 1 })
        ));
        assert!(json.contains("\"modeset\": false"));
    }

    #[test]
    fn the_xwayland_proof_is_an_optional_section() {
        use crate::wayland_x11::{GlamorRoute, XwaylandProof};
        let json = serde_json::to_string(&record()).unwrap();
        assert!(!json.contains("x11_apps"));
        let mut proven = record();
        proven.facts.x11_apps = Some(XwaylandProof {
            display_proven: true,
            window_manager: true,
            pointer: true,
            glamor: GlamorRoute::NvidiaGbm,
            xwayland_version: Some("24.1.9".into()),
            failure: None,
        });
        let json = serde_json::to_string(&proven).unwrap();
        assert!(json.contains("\"x11_apps\":{\"display_proven\":true"));
        let parsed: WaylandQualificationRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, proven);
        let validated = parsed.validate(&identity()).unwrap();
        assert!(
            validated
                .record()
                .facts
                .x11_apps
                .as_ref()
                .is_some_and(XwaylandProof::proven)
        );
    }

    #[test]
    fn speed_is_decided_on_the_route_the_host_serves() {
        assert_eq!(speed_minimum_fps(60), 57);
        let request = |route| SpeedRequest {
            width: 1920,
            height: 1080,
            codec: "h265",
            route,
            candidate_refreshes: &[60, 120],
            ceiling_fps: 60,
            minimum_fps: speed_minimum_fps(60),
        };
        let host = record().validate(&identity()).unwrap();
        // A host copy record serves on the host copy.
        assert_eq!(host.serving_route(), CaptureRoute::HostCopy);
        let decision = host.speed_decision(&request(host.serving_route())).unwrap();
        assert_eq!(decision.output_refresh_hz, 60);
        assert_eq!(decision.capture_rate, 60);
        assert_eq!(decision.measured_fps, 58);
        assert_eq!(
            host.speed_decision(&request(CaptureRoute::GpuConvert)),
            None
        );
        // A verified GPU route serves on it.
        let mut gpu = record();
        let proof = gpu.facts.capture_route.as_mut().unwrap();
        proof.route = CaptureRoute::GpuConvert;
        proof.gpu_import_verified_frames = 1;
        let host = gpu.validate(&identity()).unwrap();
        assert_eq!(host.serving_route(), CaptureRoute::GpuConvert);
        // An inferred entry in a record is never evidence.
        let mut inferred = record();
        inferred.facts.source_rates[0].evidence = RateEvidence::Inferred {
            anchor_width: 3840,
            anchor_height: 2160,
        };
        let host = inferred.validate(&identity()).unwrap();
        assert_eq!(host.speed_decision(&request(CaptureRoute::HostCopy)), None);
    }

    #[test]
    fn versions_are_read_from_what_the_tools_print() {
        assert_eq!(
            parse_compositor_version("GNOME Shell 49.4\n").as_deref(),
            Some("49.4")
        );
        assert_eq!(
            parse_compositor_version("mutter 40.9\n").as_deref(),
            Some("40.9")
        );
        assert_eq!(parse_compositor_version("mutter\n"), None);
        assert_eq!(
            parse_pipewire_version(
                "pipewire\nCompiled with libpipewire 1.4.10\nLinked with libpipewire 1.4.11\n"
            )
            .as_deref(),
            Some("1.4.11")
        );
        assert_eq!(
            parse_pipewire_version("pipewire\nCompiled with libpipewire 1.0.1\n").as_deref(),
            Some("1.0.1")
        );
        assert_eq!(parse_pipewire_version("pipewire\n"), None);
        assert_eq!(
            parse_os_release(
                "NAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel\"\nVERSION_ID=\"10.2\"\n"
            ),
            ("rocky".to_owned(), "10.2".to_owned())
        );
    }
}
