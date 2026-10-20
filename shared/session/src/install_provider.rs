//! Install-time desktop-provider plan for the Linux Pier installer.
//!
//! The installer decides, before it writes anything, which desktop this
//! host will serve, and explains it. It follows the Pier's start-up rules
//! ([`crate::desktop_provider`]) with the facts that exist before any
//! session does:
//!
//! - an Xorg server is installed → X11, the existing path; an Xorg
//!   installation is never moved to Wayland;
//! - no Xorg server, and a headless GNOME Shell or Mutter, systemd-logind,
//!   `dbus-daemon`, `PipeWire`, `WirePlumber` and a DRM render node a
//!   session user can open → Wayland, *pending* the session-proven facts
//!   that `arcen-pier wayland qualify --write` records (the installer runs
//!   it; only a passing run lets the service start);
//! - no Xorg server and no DRM render node → refused, naming the GPU driver
//!   to install (for NVIDIA, the newest driver this kernel needs);
//! - no Xorg server and render nodes that only a group can open → refused,
//!   saying how to open them (a headless session has no seat, so logind
//!   grants it no device access);
//! - anything else a Wayland desktop needs is missing → refused, naming it.
//!
//! `capture.provider` (`xorg`/`wayland`) overrides the choice exactly as it
//! does at Pier start. The installer never changes driver, kernel, device
//! permission, group or boot configuration, nor the `SELinux` mode (it loads
//! only Arcen's own policy module), and nothing decides by distribution
//! name.
//!
//! Pure policy. The installer gathers the facts; this decides and words
//! the explanation.

use std::fmt::{Display, Formatter};

use crate::desktop_provider::{CapabilityFact, ProviderPreference};
use crate::wayland_qualification::{GpuDriver, QUALIFY_COMMAND};
use crate::wayland_session::{
    DBUS_DAEMON_BINARY, LOGIND_SEATS_DIR, PIPEWIRE_BINARY, WAYLAND_HOST_COMPOSITORS,
    WIREPLUMBER_BINARY, WaylandHostProbe,
};

/// First NVIDIA driver whose `nvidia-drm` builds against a kernel whose DRM
/// `fb_create` hook takes a `drm_format_info` argument (the
/// `NV_DRM_FB_CREATE_TAKES_FORMAT_INFO` conftest first ships in 580.82.07).
pub const NVIDIA_FORMAT_INFO_FB_CREATE_MINIMUM: [u32; 3] = [580, 82, 7];

/// Its printed form.
pub const NVIDIA_FORMAT_INFO_FB_CREATE_MINIMUM_TEXT: &str = "580.82.07";

/// The kernel release where mainline `fb_create` gained the format-info
/// argument. Distribution kernels may carry it earlier (backports), which
/// only the kernel headers can show.
pub const FB_CREATE_FORMAT_INFO_MAINLINE: (u32, u32) = (6, 17);

/// One `/dev/dri/renderD*` node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderNode {
    /// Device path.
    pub path: String,
    /// Readable and writable by every user, so an unprivileged session can
    /// open it without a group Arcen would have to add.
    pub world_accessible: bool,
    /// Permission bits, when the node could be inspected.
    pub mode: Option<u32>,
    /// Owning group name, when known.
    pub group: Option<String>,
    /// PCI address of the GPU behind it, when sysfs names one.
    pub pci_address: Option<String>,
    /// PCI vendor id of that GPU.
    pub vendor_id: Option<u16>,
}

/// Whether a session user can open a render node: some node is open to
/// everyone → available; nodes exist but none is → unknown (Arcen never
/// adds the group); none exist → missing. The Pier's start-up probe uses
/// the same rule.
#[must_use]
pub fn render_node_fact(nodes: &[RenderNode]) -> CapabilityFact {
    if nodes.iter().any(|node| node.world_accessible) {
        CapabilityFact::Available
    } else if nodes.is_empty() {
        CapabilityFact::Missing
    } else {
        CapabilityFact::Unknown
    }
}

/// A readable name for a PCI vendor id.
#[must_use]
pub const fn pci_vendor_name(vendor_id: u16) -> Option<&'static str> {
    match vendor_id {
        0x10de => Some("NVIDIA"),
        0x1002 => Some("AMD"),
        0x8086 => Some("Intel"),
        0x1af4 => Some("virtio"),
        0x1b36 => Some("QEMU/QXL"),
        0x1234 => Some("QEMU/Bochs"),
        0x15ad => Some("VMware"),
        0x1414 => Some("Hyper-V"),
        0x102b => Some("Matrox"),
        0x1a03 => Some("ASPEED"),
        _ => None,
    }
}

/// Where `capture.provider` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferenceSource {
    /// No configuration on the host yet, or the key is absent: `auto`.
    Default,
    /// The existing `/etc/arcen/pier.json`, kept on upgrade.
    ExistingConfig,
}

/// Everything the installer establishes about the host before it writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallHostFacts {
    /// `capture.provider` the installed Pier will run with.
    pub preference: ProviderPreference,
    /// Where that came from.
    pub preference_source: PreferenceSource,
    /// The installed Xorg server executable, if any.
    pub xorg_server: Option<String>,
    /// The embedded Pier contains the headless Wayland provider.
    pub wayland_built: bool,
    /// Static Wayland facts; the session-proven ones stay unknown here.
    pub probe: WaylandHostProbe,
    /// Version the chosen compositor reports.
    pub compositor_version: Option<String>,
    /// `PipeWire` library version `pipewire --version` reports.
    pub pipewire_version: Option<String>,
    /// A `PulseAudio` server (`pipewire-pulse` or `pulseaudio`) for session
    /// audio, if installed.
    pub pulse_server: Option<String>,
    /// Every DRM render node.
    pub render_nodes: Vec<RenderNode>,
    /// The loaded NVIDIA driver, if any.
    pub gpu_driver: Option<GpuDriver>,
    /// Running kernel release.
    pub kernel_release: String,
    /// Whether this kernel's DRM `fb_create` takes a format-info argument:
    /// from its headers when installed, else from the mainline version.
    pub kernel_fb_create_format_info: Option<bool>,
}

/// What a Wayland desktop needs that this host lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaylandComponent {
    /// No GNOME Shell or Mutter that runs headless with virtual monitors.
    Compositor {
        /// A compositor binary is installed but does not advertise both
        /// modes.
        installed_without_headless: bool,
    },
    /// systemd-logind is not running.
    Logind,
    /// No `dbus-daemon` for the session's private bus.
    DbusDaemon,
    /// No `PipeWire` daemon.
    PipeWire,
    /// No `WirePlumber` session manager.
    WirePlumber,
    /// No DRM render node a session user can open.
    RenderNode(CapabilityFact),
}

impl WaylandComponent {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Compositor { .. } => "compositor",
            Self::Logind => "logind",
            Self::DbusDaemon => "dbus_daemon",
            Self::PipeWire => "pipewire",
            Self::WirePlumber => "wireplumber",
            Self::RenderNode(_) => "render_node_access",
        }
    }
}

impl Display for WaylandComponent {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compositor {
                installed_without_headless: true,
            } => formatter.write_str(
                "the installed GNOME Shell/Mutter does not offer --headless and \
                 --virtual-monitor: install GNOME Shell 40 or newer",
            ),
            Self::Compositor {
                installed_without_headless: false,
            } => write!(
                formatter,
                "no GNOME Shell or Mutter is installed ({}): install GNOME Shell 40 or newer",
                WAYLAND_HOST_COMPOSITORS
                    .iter()
                    .map(|compositor| compositor.binary())
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
            Self::Logind => write!(
                formatter,
                "systemd-logind is not running ({LOGIND_SEATS_DIR} is absent): each desktop is \
                 its own logind session"
            ),
            Self::DbusDaemon => write!(
                formatter,
                "{DBUS_DAEMON_BINARY} is not installed: the session runs a private bus with the \
                 reference D-Bus daemon (dbus-broker alone is not enough)"
            ),
            Self::PipeWire => write!(
                formatter,
                "{PIPEWIRE_BINARY} is not installed: the desktop is captured through PipeWire"
            ),
            Self::WirePlumber => write!(
                formatter,
                "{WIREPLUMBER_BINARY} is not installed: PipeWire's session manager"
            ),
            Self::RenderNode(CapabilityFact::Unknown) => formatter.write_str(
                "/dev/dri/renderD* exists but no render node is readable and writable by every \
                 user; Arcen does not change device permissions or group membership",
            ),
            Self::RenderNode(_) => formatter.write_str(
                "no DRM render node (/dev/dri/renderD*): the GPU driver has no DRM/KMS (any \
                     vendor's render node qualifies)",
            ),
        }
    }
}

/// Whether this kernel needs a newer NVIDIA driver for `nvidia-drm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelDrmApi {
    /// The loaded driver supports this kernel's DRM API, or it does not
    /// matter.
    Supported,
    /// This kernel's `fb_create` takes a format-info argument (headers or
    /// version show it) and the driver predates support for it.
    NeedsNewerDriver,
    /// The driver predates that support and the kernel's API could not be
    /// established (no headers installed).
    MayNeedNewerDriver,
}

/// What to do about a host whose GPU has no usable DRM render node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverAdvice {
    /// The NVIDIA driver is loaded but its DRM module is not.
    NvidiaWithoutDrm {
        /// Loaded driver version.
        version: String,
        /// Kernel release.
        kernel: String,
        /// Whether `nvidia-drm` can build for this kernel with this driver.
        kernel_api: KernelDrmApi,
    },
    /// `nvidia-drm` is loaded but no render node is usable.
    NvidiaDrmWithoutRenderNode {
        /// Loaded driver version.
        version: String,
    },
    /// No NVIDIA driver is loaded.
    NoNvidiaDriver,
}

impl Display for DriverAdvice {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NvidiaWithoutDrm {
                version,
                kernel,
                kernel_api,
            } => {
                write!(
                    formatter,
                    "the NVIDIA driver {version} is loaded without its DRM module (nvidia-drm), \
                     so the GPU has no DRM render node. Install the driver with its nvidia-drm \
                     module and set `options nvidia-drm modeset=1` (for example in a file under \
                     /etc/modprobe.d/), regenerate the initramfs if the driver loads from it, \
                     and reboot."
                )?;
                match kernel_api {
                    KernelDrmApi::Supported => Ok(()),
                    KernelDrmApi::NeedsNewerDriver => write!(
                        formatter,
                        " This kernel ({kernel}) has the newer DRM fb_create interface, which \
                         NVIDIA drivers before {NVIDIA_FORMAT_INFO_FB_CREATE_MINIMUM_TEXT} \
                         cannot build nvidia-drm against: install \
                         {NVIDIA_FORMAT_INFO_FB_CREATE_MINIMUM_TEXT} or newer."
                    ),
                    KernelDrmApi::MayNeedNewerDriver => write!(
                        formatter,
                        " If nvidia-drm fails to build for this kernel ({kernel}): kernels whose \
                         DRM fb_create takes a format-info argument (Linux {}.{}, and \
                         distribution kernels that backport it) need NVIDIA \
                         {NVIDIA_FORMAT_INFO_FB_CREATE_MINIMUM_TEXT} or newer.",
                        FB_CREATE_FORMAT_INFO_MAINLINE.0, FB_CREATE_FORMAT_INFO_MAINLINE.1
                    ),
                }
            }
            Self::NvidiaDrmWithoutRenderNode { version } => write!(
                formatter,
                "the NVIDIA driver {version} and nvidia-drm are loaded, but no render node is \
                 usable by a session user; check /dev/dri and the driver's installation"
            ),
            Self::NoNvidiaDriver => formatter.write_str(
                "install a GPU driver with DRM/KMS: Intel (i915 or xe) and AMD (amdgpu) render \
                 nodes qualify, and a host without a hardware encoder is served in software \
                 (OpenH264); for NVIDIA, the driver with its nvidia-drm module and \
                 `options nvidia-drm modeset=1`",
            ),
        }
    }
}

/// Parses a dotted driver version (`580.105.08`).
fn version_numbers(version: &str) -> Option<Vec<u32>> {
    version
        .trim()
        .split('.')
        .map(|part| part.parse().ok())
        .collect()
}

/// Whether `version` is older than `minimum`. `None` when it does not parse.
#[must_use]
pub fn driver_version_before(version: &str, minimum: &[u32]) -> Option<bool> {
    let numbers = version_numbers(version)?;
    (!numbers.is_empty()).then(|| numbers.as_slice() < minimum)
}

/// Whether a kernel release is at least `major.minor`.
#[must_use]
pub fn kernel_at_least(release: &str, major: u32, minor: u32) -> Option<bool> {
    let mut parts = release
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty());
    let found_major: u32 = parts.next()?.parse().ok()?;
    let found_minor: u32 = parts.next()?.parse().ok()?;
    Some((found_major, found_minor) >= (major, minor))
}

/// Whether `drm_mode_config.h` declares `fb_create` with a format-info
/// argument. `None` when the header names no `fb_create` hook.
#[must_use]
pub fn header_fb_create_takes_format_info(header: &str) -> Option<bool> {
    let start = header.find("(*fb_create)")?;
    let declaration = &header[start..];
    let end = declaration.find(';')?;
    Some(declaration[..end].contains("drm_format_info"))
}

/// The kernel's `fb_create` interface: its headers decide when installed,
/// otherwise the mainline version proves only the newer interface.
#[must_use]
pub fn kernel_fb_create_format_info(release: &str, header: Option<&str>) -> Option<bool> {
    if let Some(from_header) = header.and_then(header_fb_create_takes_format_info) {
        return Some(from_header);
    }
    let (major, minor) = FB_CREATE_FORMAT_INFO_MAINLINE;
    kernel_at_least(release, major, minor).filter(|newer| *newer)
}

impl InstallHostFacts {
    /// The render-node fact.
    #[must_use]
    pub fn render_node(&self) -> CapabilityFact {
        render_node_fact(&self.render_nodes)
    }

    /// What the GPU driver needs when there is no usable render node.
    #[must_use]
    pub fn driver_advice(&self) -> DriverAdvice {
        match &self.gpu_driver {
            Some(driver) if driver.vendor == "nvidia" && !driver.drm => {
                let kernel_api = match driver_version_before(
                    &driver.version,
                    &NVIDIA_FORMAT_INFO_FB_CREATE_MINIMUM,
                ) {
                    Some(true) => match self.kernel_fb_create_format_info {
                        Some(true) => KernelDrmApi::NeedsNewerDriver,
                        Some(false) => KernelDrmApi::Supported,
                        None => KernelDrmApi::MayNeedNewerDriver,
                    },
                    Some(false) | None => KernelDrmApi::Supported,
                };
                DriverAdvice::NvidiaWithoutDrm {
                    version: driver.version.clone(),
                    kernel: self.kernel_release.clone(),
                    kernel_api,
                }
            }
            Some(driver) if driver.vendor == "nvidia" => DriverAdvice::NvidiaDrmWithoutRenderNode {
                version: driver.version.clone(),
            },
            _ => DriverAdvice::NoNvidiaDriver,
        }
    }

    /// Every static Wayland prerequisite this host lacks, in a fixed order.
    #[must_use]
    pub fn wayland_missing(&self) -> Vec<WaylandComponent> {
        let mut missing = Vec::new();
        if self.probe.compositor().is_none() {
            missing.push(WaylandComponent::Compositor {
                installed_without_headless: !self.probe.compositors.is_empty(),
            });
        }
        if !self.probe.logind {
            missing.push(WaylandComponent::Logind);
        }
        if !self.probe.dbus_daemon {
            missing.push(WaylandComponent::DbusDaemon);
        }
        if !self.probe.pipewire {
            missing.push(WaylandComponent::PipeWire);
        }
        if !self.probe.session_manager {
            missing.push(WaylandComponent::WirePlumber);
        }
        let render_node = self.render_node();
        if render_node != CapabilityFact::Available {
            missing.push(WaylandComponent::RenderNode(render_node));
        }
        missing
    }
}

/// Why the installer chose its provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReason {
    /// `auto` with an Xorg server installed.
    XorgServerInstalled,
    /// `auto` without an Xorg server.
    NoXorgServer,
    /// `capture.provider` names the provider.
    OperatorOverride,
}

impl PlanReason {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::XorgServerInstalled => "xorg_server_installed",
            Self::NoXorgServer => "no_xorg_server",
            Self::OperatorOverride => "operator_override",
        }
    }
}

/// What the installer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPlan {
    /// The dedicated X11 desktop: the existing installation path.
    Xorg {
        /// Why.
        reason: PlanReason,
    },
    /// The headless Wayland desktop, once `wayland qualify --write`
    /// passes; the service starts only then.
    Wayland {
        /// Why.
        reason: PlanReason,
    },
}

impl InstallPlan {
    /// Stable provider token.
    #[must_use]
    pub const fn provider_token(self) -> &'static str {
        match self {
            Self::Xorg { .. } => "xorg",
            Self::Wayland { .. } => "wayland",
        }
    }

    /// Why.
    #[must_use]
    pub const fn reason(self) -> PlanReason {
        match self {
            Self::Xorg { reason } | Self::Wayland { reason } => reason,
        }
    }
}

impl Display for InstallPlan {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Xorg {
                reason: PlanReason::OperatorOverride,
            } => formatter.write_str("X11 (capture.provider = \"xorg\")"),
            Self::Xorg { .. } => formatter.write_str(
                "X11: an Xorg server is installed (the existing path; an Xorg host is never \
                 moved to Wayland)",
            ),
            Self::Wayland {
                reason: PlanReason::OperatorOverride,
            } => write!(
                formatter,
                "Wayland (capture.provider = \"wayland\"), once `{QUALIFY_COMMAND}` passes"
            ),
            Self::Wayland { .. } => write!(
                formatter,
                "Wayland: no Xorg server, and the headless GNOME stack and a DRM render node are \
                 present; the service starts once `{QUALIFY_COMMAND}` passes"
            ),
        }
    }
}

/// Why the installer stops before writing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallRefusal {
    /// `capture.provider = "xorg"` and no Xorg server.
    XorgOverrideWithoutXorg,
    /// Wayland is needed and the embedded Pier was built without it.
    WaylandNotBuilt {
        /// `capture.provider = "wayland"`.
        requested: bool,
    },
    /// `auto`, no Xorg server, no usable DRM render node.
    NoDisplayStack {
        /// Every Wayland prerequisite missing, render node included.
        missing: Vec<WaylandComponent>,
        /// What to do about the GPU driver.
        advice: DriverAdvice,
    },
    /// Wayland is needed, render nodes exist, but only a group can open
    /// them; a seatless headless session gets no logind device access.
    RenderNodeRestricted {
        /// `capture.provider = "wayland"`.
        requested: bool,
        /// The render nodes.
        nodes: Vec<RenderNode>,
        /// Other missing prerequisites.
        missing: Vec<WaylandComponent>,
    },
    /// Wayland is needed and some prerequisite is missing.
    WaylandIncomplete {
        /// `capture.provider = "wayland"`.
        requested: bool,
        /// Every missing prerequisite.
        missing: Vec<WaylandComponent>,
        /// Driver advice when the render node is among them.
        advice: Option<DriverAdvice>,
    },
}

impl InstallRefusal {
    /// Stable token.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::XorgOverrideWithoutXorg => "xorg_override_without_xorg",
            Self::WaylandNotBuilt { .. } => "wayland_not_built",
            Self::NoDisplayStack { .. } => "no_display_stack",
            Self::RenderNodeRestricted { .. } => "render_node_restricted",
            Self::WaylandIncomplete { .. } => "wayland_incomplete",
        }
    }
}

impl Display for InstallRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let list = |formatter: &mut Formatter<'_>, missing: &[WaylandComponent]| {
            missing
                .iter()
                .try_for_each(|component| write!(formatter, "\n  - {component}"))
        };
        match self {
            Self::XorgOverrideWithoutXorg => formatter.write_str(
                "capture.provider is \"xorg\" in /etc/arcen/pier.json, but no Xorg server is \
                 installed: install an Xorg server, or set capture.provider to \"auto\"",
            ),
            Self::WaylandNotBuilt { requested } => {
                if *requested {
                    formatter.write_str("capture.provider is \"wayland\"")?;
                } else {
                    formatter.write_str("no Xorg server is installed")?;
                }
                formatter.write_str(
                    ", and this installer's Pier was built without the Wayland provider: \
                     install an Xorg server, or use a release built with it",
                )
            }
            Self::NoDisplayStack { missing, advice } => {
                write!(
                    formatter,
                    "this host has neither an Xorg server nor a DRM render node, so Arcen has \
                     no desktop to serve. With an Xorg server it serves a dedicated X11 \
                     desktop; with a GPU driver that has DRM/KMS it can serve a headless \
                     Wayland desktop.\n\nFor Wayland: {advice}\n\nMissing for Wayland:"
                )?;
                list(formatter, missing)?;
                formatter.write_str(
                    "\n\nArcen does not change driver, kernel-module, kernel or Secure Boot \
                     configuration. Fix the host and run the installer again.",
                )
            }
            Self::RenderNodeRestricted {
                requested,
                nodes,
                missing,
            } => {
                if *requested {
                    formatter.write_str("capture.provider is \"wayland\", but")?;
                } else {
                    formatter.write_str("no Xorg server is installed, and")?;
                }
                formatter.write_str(
                    " the GPU's DRM render node is open only to a group, not to every user:",
                )?;
                for node in nodes {
                    write!(formatter, "\n  - {}", render_node_access(node))?;
                }
                formatter.write_str(
                    "\n\nArcen runs each Wayland desktop as a headless logind session without a \
                     seat, so logind grants it no device access, and the Pier accepts a render \
                     node only when every session user can open it. Make the render nodes \
                     readable and writable by every user (the systemd default, for example a \
                     udev rule `SUBSYSTEM==\"drm\", KERNEL==\"renderD*\", MODE=\"0666\"` in \
                     /etc/udev/rules.d/, then `udevadm trigger`), or install an Xorg server for \
                     the dedicated X11 desktop. Arcen does not change device permissions or group \
                     membership itself.",
                )?;
                if !missing.is_empty() {
                    formatter.write_str("\n\nAlso missing for Wayland:")?;
                    list(formatter, missing)?;
                }
                Ok(())
            }
            Self::WaylandIncomplete {
                requested,
                missing,
                advice,
            } => {
                if *requested {
                    formatter.write_str(
                        "capture.provider is \"wayland\", but this host lacks what a headless \
                         Wayland desktop needs:",
                    )?;
                } else {
                    formatter.write_str(
                        "no Xorg server is installed, and this host lacks what a headless \
                         Wayland desktop needs:",
                    )?;
                }
                list(formatter, missing)?;
                if let Some(advice) = advice {
                    write!(formatter, "\n\nGPU driver: {advice}")?;
                }
                formatter.write_str(
                    "\n\nInstall what is missing (or an Xorg server, for an X11 desktop) and run \
                     the installer again. Arcen does not install system packages or change \
                     driver configuration.",
                )
            }
        }
    }
}

impl std::error::Error for InstallRefusal {}

/// Decides what the installer does.
///
/// # Errors
///
/// [`InstallRefusal`]: the installer stops before writing anything.
pub fn plan_install_provider(facts: &InstallHostFacts) -> Result<InstallPlan, InstallRefusal> {
    let xorg = facts.xorg_server.is_some();
    let requested = match facts.preference {
        ProviderPreference::Xorg if xorg => {
            return Ok(InstallPlan::Xorg {
                reason: PlanReason::OperatorOverride,
            });
        }
        ProviderPreference::Xorg => return Err(InstallRefusal::XorgOverrideWithoutXorg),
        ProviderPreference::Auto if xorg => {
            return Ok(InstallPlan::Xorg {
                reason: PlanReason::XorgServerInstalled,
            });
        }
        ProviderPreference::Auto => false,
        ProviderPreference::Wayland => true,
    };
    if !facts.wayland_built {
        return Err(InstallRefusal::WaylandNotBuilt { requested });
    }
    let missing = facts.wayland_missing();
    if missing.is_empty() {
        return Ok(InstallPlan::Wayland {
            reason: if requested {
                PlanReason::OperatorOverride
            } else {
                PlanReason::NoXorgServer
            },
        });
    }
    if facts.render_node() == CapabilityFact::Unknown {
        return Err(InstallRefusal::RenderNodeRestricted {
            requested,
            nodes: facts.render_nodes.clone(),
            missing: missing
                .into_iter()
                .filter(|component| !matches!(component, WaylandComponent::RenderNode(_)))
                .collect(),
        });
    }
    let render_node_missing = missing
        .iter()
        .any(|component| matches!(component, WaylandComponent::RenderNode(_)));
    if !requested && render_node_missing {
        return Err(InstallRefusal::NoDisplayStack {
            missing,
            advice: facts.driver_advice(),
        });
    }
    Err(InstallRefusal::WaylandIncomplete {
        requested,
        advice: render_node_missing.then(|| facts.driver_advice()),
        missing,
    })
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn compositor_summary(facts: &InstallHostFacts) -> String {
    match facts.probe.compositor() {
        Some(compositor) => format!(
            "{} {} (headless, virtual monitors)",
            compositor.token(),
            facts
                .compositor_version
                .as_deref()
                .unwrap_or("version unknown")
        ),
        None if facts.probe.compositors.is_empty() => "none (GNOME Shell/Mutter)".to_owned(),
        None => format!(
            "{} without --headless/--virtual-monitor",
            facts
                .probe
                .compositors
                .iter()
                .map(|probe| probe.compositor.token())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `mode 0660, group render` for a node, as far as known.
fn render_node_permissions(node: &RenderNode) -> String {
    match (node.mode, &node.group) {
        (Some(mode), Some(group)) => format!("mode {:04o}, group {group}", mode & 0o7777),
        (Some(mode), None) => format!("mode {:04o}", mode & 0o7777),
        (None, Some(group)) => format!("group {group}"),
        (None, None) => "permissions unknown".to_owned(),
    }
}

/// A node and who can open it, for the restricted-node refusal.
fn render_node_access(node: &RenderNode) -> String {
    format!("{} ({})", node.path, render_node_permissions(node))
}

fn render_node_summary(node: &RenderNode) -> String {
    let gpu = match (&node.pci_address, node.vendor_id) {
        (Some(pci), Some(vendor)) => format!(
            "GPU {pci}, {} 0x{vendor:04x}",
            pci_vendor_name(vendor).unwrap_or("vendor")
        ),
        (Some(pci), None) => format!("GPU {pci}"),
        (None, _) => "GPU unknown".to_owned(),
    };
    format!(
        "{} ({gpu}){}",
        node.path,
        if node.world_accessible {
            ", open to session users".to_owned()
        } else {
            format!(
                ", {}: not open to every user",
                render_node_permissions(node)
            )
        }
    )
}

/// The detection summary the installer prints and logs: one fact per line.
#[must_use]
pub fn summary_lines(
    facts: &InstallHostFacts,
    decision: &Result<InstallPlan, InstallRefusal>,
) -> Vec<String> {
    let mut lines = vec!["Desktop provider detection:".to_owned()];
    let mut fact = |name: &str, value: String| lines.push(format!("  {name:<18}{value}"));
    fact(
        "Xorg server:",
        facts
            .xorg_server
            .clone()
            .unwrap_or_else(|| "not installed".to_owned()),
    );
    fact("Compositor:", compositor_summary(facts));
    fact(
        "Session services:",
        format!(
            "systemd-logind {}, dbus-daemon {}",
            yes_no(facts.probe.logind),
            yes_no(facts.probe.dbus_daemon)
        ),
    );
    fact(
        "PipeWire:",
        format!(
            "{}, WirePlumber {}, PulseAudio server {}",
            if facts.probe.pipewire {
                facts
                    .pipewire_version
                    .clone()
                    .unwrap_or_else(|| "installed".to_owned())
            } else {
                "not installed".to_owned()
            },
            yes_no(facts.probe.session_manager),
            facts.pulse_server.as_deref().unwrap_or("none")
        ),
    );
    if facts.render_nodes.is_empty() {
        fact("DRM render node:", "none".to_owned());
    }
    for node in &facts.render_nodes {
        fact("DRM render node:", render_node_summary(node));
    }
    fact(
        "NVIDIA driver:",
        match &facts.gpu_driver {
            Some(driver) => format!(
                "{}, nvidia_drm {}, modeset {}",
                driver.version,
                if driver.drm { "loaded" } else { "not loaded" },
                match driver.modeset {
                    Some(true) => "Y",
                    Some(false) => "N",
                    None => "unknown",
                }
            ),
            None => "not loaded".to_owned(),
        },
    );
    fact(
        "Kernel:",
        format!(
            "{}{}",
            facts.kernel_release,
            match facts.kernel_fb_create_format_info {
                Some(true) => " (DRM fb_create takes format info)",
                Some(false) => " (DRM fb_create without format info)",
                None => "",
            }
        ),
    );
    fact(
        "capture.provider:",
        format!(
            "{} ({})",
            facts.preference.token(),
            match facts.preference_source {
                PreferenceSource::Default => "default",
                PreferenceSource::ExistingConfig => "from /etc/arcen/pier.json",
            }
        ),
    );
    fact(
        "Decision:",
        match decision {
            Ok(plan) => plan.to_string(),
            Err(refusal) => format!("none, the installer stops ({})", refusal.token()),
        },
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland_session::{CompositorProbe, HeadlessCompositor};

    fn nvidia(version: &str, drm: bool, modeset: Option<bool>) -> GpuDriver {
        GpuDriver {
            vendor: "nvidia".to_owned(),
            version: version.to_owned(),
            drm,
            modeset,
        }
    }

    fn node(world_accessible: bool) -> RenderNode {
        RenderNode {
            path: "/dev/dri/renderD128".to_owned(),
            world_accessible,
            mode: Some(if world_accessible { 0o666 } else { 0o660 }),
            group: Some("render".to_owned()),
            pci_address: Some("0000:00:10.0".to_owned()),
            vendor_id: Some(0x10de),
        }
    }

    /// Rocky 10.2 / GNOME 49 / GRID 580.105.08 with nvidia-drm modeset=1.
    fn wayland_host() -> InstallHostFacts {
        InstallHostFacts {
            preference: ProviderPreference::Auto,
            preference_source: PreferenceSource::Default,
            xorg_server: None,
            wayland_built: true,
            probe: WaylandHostProbe {
                compositors: vec![CompositorProbe {
                    compositor: HeadlessCompositor::GnomeShell,
                    headless: true,
                    virtual_monitor: true,
                }],
                logind: true,
                dbus_daemon: true,
                pipewire: true,
                session_manager: true,
                render_node: CapabilityFact::Available,
                ..WaylandHostProbe::default()
            },
            compositor_version: Some("49.4".to_owned()),
            pipewire_version: Some("1.4.11".to_owned()),
            pulse_server: Some("/usr/bin/pipewire-pulse".to_owned()),
            render_nodes: vec![node(true)],
            gpu_driver: Some(nvidia("580.105.08", true, Some(true))),
            kernel_release: "6.12.0-211.66.1.el10_2.x86_64".to_owned(),
            kernel_fb_create_format_info: Some(true),
        }
    }

    fn xorg_host() -> InstallHostFacts {
        InstallHostFacts {
            xorg_server: Some("/usr/libexec/Xorg".to_owned()),
            ..wayland_host()
        }
    }

    #[test]
    fn an_xorg_server_keeps_x11_whatever_wayland_offers() {
        assert_eq!(
            plan_install_provider(&xorg_host()),
            Ok(InstallPlan::Xorg {
                reason: PlanReason::XorgServerInstalled
            })
        );
        let mut bare = xorg_host();
        bare.wayland_built = false;
        bare.render_nodes.clear();
        bare.probe = WaylandHostProbe::default();
        assert_eq!(
            plan_install_provider(&bare),
            Ok(InstallPlan::Xorg {
                reason: PlanReason::XorgServerInstalled
            })
        );
    }

    #[test]
    fn no_xorg_with_the_stack_and_drm_qualifies_wayland() {
        let plan = plan_install_provider(&wayland_host()).unwrap();
        assert_eq!(
            plan,
            InstallPlan::Wayland {
                reason: PlanReason::NoXorgServer
            }
        );
        assert!(plan.to_string().contains(QUALIFY_COMMAND));
    }

    #[test]
    fn the_operator_override_is_respected_both_ways() {
        let mut host = xorg_host();
        host.preference = ProviderPreference::Wayland;
        host.preference_source = PreferenceSource::ExistingConfig;
        assert_eq!(
            plan_install_provider(&host),
            Ok(InstallPlan::Wayland {
                reason: PlanReason::OperatorOverride
            })
        );
        host.preference = ProviderPreference::Xorg;
        assert_eq!(
            plan_install_provider(&host),
            Ok(InstallPlan::Xorg {
                reason: PlanReason::OperatorOverride
            })
        );
        let mut wayland = wayland_host();
        wayland.preference = ProviderPreference::Xorg;
        let refusal = plan_install_provider(&wayland).unwrap_err();
        assert_eq!(refusal, InstallRefusal::XorgOverrideWithoutXorg);
        assert!(
            refusal
                .to_string()
                .contains("set capture.provider to \"auto\"")
        );
    }

    #[test]
    fn nvidia_without_drm_stops_and_names_the_driver_for_this_kernel() {
        let mut host = wayland_host();
        host.render_nodes.clear();
        host.gpu_driver = Some(nvidia("580.65.06", false, None));
        let refusal = plan_install_provider(&host).unwrap_err();
        let InstallRefusal::NoDisplayStack { missing, advice } = &refusal else {
            panic!("{refusal:?}");
        };
        assert_eq!(
            missing,
            &[WaylandComponent::RenderNode(CapabilityFact::Missing)]
        );
        assert!(matches!(
            advice,
            DriverAdvice::NvidiaWithoutDrm {
                kernel_api: KernelDrmApi::NeedsNewerDriver,
                ..
            }
        ));
        let text = refusal.to_string();
        assert!(text.contains("options nvidia-drm modeset=1"), "{text}");
        assert!(text.contains("580.82.07 or newer"), "{text}");
        assert!(text.contains("does not change driver"), "{text}");
    }

    #[test]
    fn the_newer_driver_is_named_only_when_this_kernel_needs_it() {
        let advice = |version: &str, api: Option<bool>| {
            let mut host = wayland_host();
            host.render_nodes.clear();
            host.gpu_driver = Some(nvidia(version, false, None));
            host.kernel_fb_create_format_info = api;
            host.driver_advice()
        };
        let api = |advice: DriverAdvice| match advice {
            DriverAdvice::NvidiaWithoutDrm { kernel_api, .. } => kernel_api,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            api(advice("580.76.05", Some(true))),
            KernelDrmApi::NeedsNewerDriver
        );
        assert_eq!(
            api(advice("580.76.05", None)),
            KernelDrmApi::MayNeedNewerDriver
        );
        assert_eq!(
            api(advice("580.76.05", Some(false))),
            KernelDrmApi::Supported
        );
        assert_eq!(
            api(advice("580.82.07", Some(true))),
            KernelDrmApi::Supported
        );
        assert_eq!(
            api(advice("580.105.08", Some(true))),
            KernelDrmApi::Supported
        );
        assert_eq!(
            api(advice("unparsable", Some(true))),
            KernelDrmApi::Supported
        );
        let maybe = advice("570.172.08", None).to_string();
        assert!(maybe.contains("If nvidia-drm fails to build"), "{maybe}");
        assert!(
            !advice("580.105.08", Some(true))
                .to_string()
                .contains("580.82.07")
        );
    }

    #[test]
    fn other_hosts_without_drm_get_generic_or_nvidia_drm_advice() {
        let mut host = wayland_host();
        host.render_nodes.clear();
        host.gpu_driver = None;
        assert_eq!(host.driver_advice(), DriverAdvice::NoNvidiaDriver);
        host.gpu_driver = Some(nvidia("580.105.08", true, Some(false)));
        assert!(matches!(
            host.driver_advice(),
            DriverAdvice::NvidiaDrmWithoutRenderNode { .. }
        ));
    }

    #[test]
    fn a_render_node_only_a_group_can_open_is_explained_without_driver_advice() {
        let mut host = wayland_host();
        host.render_nodes = vec![node(false)];
        host.probe.session_manager = false;
        let refusal = plan_install_provider(&host).unwrap_err();
        let InstallRefusal::RenderNodeRestricted {
            requested: false,
            nodes,
            missing,
        } = &refusal
        else {
            panic!("{refusal:?}");
        };
        assert_eq!(nodes.len(), 1);
        assert_eq!(missing, &[WaylandComponent::WirePlumber]);
        let text = refusal.to_string();
        assert!(
            text.contains("/dev/dri/renderD128 (mode 0660, group render)"),
            "{text}"
        );
        assert!(text.contains("without a seat"), "{text}");
        assert!(text.contains("MODE=\"0666\""), "{text}");
        assert!(!text.contains("neither an Xorg server nor"), "{text}");
        assert!(!text.contains("nvidia-drm"), "{text}");
        assert!(text.contains("/usr/bin/wireplumber"), "{text}");
        let summary = summary_lines(&host, &Err(refusal)).join("\n");
        assert!(
            summary.contains("mode 0660, group render: not open to every user"),
            "{summary}"
        );
        assert!(summary.contains("(render_node_restricted)"), "{summary}");

        host.probe.session_manager = true;
        host.preference = ProviderPreference::Wayland;
        assert!(matches!(
            plan_install_provider(&host),
            Err(InstallRefusal::RenderNodeRestricted {
                requested: true,
                ..
            })
        ));
    }

    #[test]
    fn a_missing_session_stack_is_listed_without_driver_advice() {
        let mut host = wayland_host();
        host.probe.compositors.clear();
        host.probe.dbus_daemon = false;
        host.probe.session_manager = false;
        let refusal = plan_install_provider(&host).unwrap_err();
        assert_eq!(
            refusal,
            InstallRefusal::WaylandIncomplete {
                requested: false,
                missing: vec![
                    WaylandComponent::Compositor {
                        installed_without_headless: false
                    },
                    WaylandComponent::DbusDaemon,
                    WaylandComponent::WirePlumber,
                ],
                advice: None,
            }
        );
        let text = refusal.to_string();
        assert!(
            text.contains("/usr/bin/gnome-shell or /usr/bin/mutter"),
            "{text}"
        );
        assert!(text.contains("/usr/bin/dbus-daemon"), "{text}");
        assert!(text.contains("/usr/bin/wireplumber"), "{text}");
    }

    #[test]
    fn a_compositor_without_headless_mode_is_named_as_such() {
        let mut host = wayland_host();
        host.probe.compositors = vec![CompositorProbe {
            compositor: HeadlessCompositor::Mutter,
            headless: true,
            virtual_monitor: false,
        }];
        host.probe.logind = false;
        host.probe.pipewire = false;
        let refusal = plan_install_provider(&host).unwrap_err();
        let InstallRefusal::WaylandIncomplete { missing, .. } = &refusal else {
            panic!("{refusal:?}");
        };
        assert_eq!(
            missing,
            &[
                WaylandComponent::Compositor {
                    installed_without_headless: true
                },
                WaylandComponent::Logind,
                WaylandComponent::PipeWire,
            ]
        );
    }

    #[test]
    fn a_wayland_override_without_drm_lists_the_render_node_with_advice() {
        let mut host = wayland_host();
        host.preference = ProviderPreference::Wayland;
        host.render_nodes.clear();
        host.gpu_driver = Some(nvidia("580.65.06", false, None));
        let refusal = plan_install_provider(&host).unwrap_err();
        let InstallRefusal::WaylandIncomplete {
            requested: true,
            advice: Some(DriverAdvice::NvidiaWithoutDrm { .. }),
            ..
        } = &refusal
        else {
            panic!("{refusal:?}");
        };
        assert!(
            refusal
                .to_string()
                .starts_with("capture.provider is \"wayland\"")
        );
    }

    #[test]
    fn a_build_without_wayland_refuses_a_host_without_xorg() {
        let mut host = wayland_host();
        host.wayland_built = false;
        assert_eq!(
            plan_install_provider(&host),
            Err(InstallRefusal::WaylandNotBuilt { requested: false })
        );
        host.preference = ProviderPreference::Wayland;
        assert_eq!(
            plan_install_provider(&host),
            Err(InstallRefusal::WaylandNotBuilt { requested: true })
        );
    }

    #[test]
    fn render_node_access_follows_the_pier_rule() {
        assert_eq!(render_node_fact(&[]), CapabilityFact::Missing);
        assert_eq!(render_node_fact(&[node(false)]), CapabilityFact::Unknown);
        assert_eq!(
            render_node_fact(&[node(false), node(true)]),
            CapabilityFact::Available
        );
    }

    #[test]
    fn kernel_and_driver_versions_compare_numerically() {
        let minimum = NVIDIA_FORMAT_INFO_FB_CREATE_MINIMUM;
        assert_eq!(driver_version_before("580.65.06", &minimum), Some(true));
        assert_eq!(driver_version_before("580.76.05", &minimum), Some(true));
        assert_eq!(driver_version_before("580.82.07", &minimum), Some(false));
        assert_eq!(driver_version_before("580.105.08", &minimum), Some(false));
        assert_eq!(driver_version_before("590.1", &minimum), Some(false));
        assert_eq!(driver_version_before("570.172.08", &minimum), Some(true));
        assert_eq!(driver_version_before("x.y", &minimum), None);
        assert_eq!(
            kernel_at_least("6.12.0-211.el10.x86_64", 6, 17),
            Some(false)
        );
        assert_eq!(kernel_at_least("6.17.1-arch1", 6, 17), Some(true));
        assert_eq!(kernel_at_least("7.0.0", 6, 17), Some(true));
        assert_eq!(kernel_at_least("garbage", 6, 17), None);
    }

    #[test]
    fn the_kernel_api_comes_from_headers_before_the_version() {
        let new = "struct drm_mode_config_funcs {\n\tstruct drm_framebuffer *(*fb_create)(struct \
                   drm_device *dev,\n\t\tstruct drm_file *file_priv,\n\t\tconst struct \
                   drm_format_info *info,\n\t\tconst struct drm_mode_fb_cmd2 *mode_cmd);\n";
        let old = "\tstruct drm_framebuffer *(*fb_create)(struct drm_device *dev,\n\t\tstruct \
                   drm_file *file_priv,\n\t\tconst struct drm_mode_fb_cmd2 *mode_cmd);\n\tconst \
                   struct drm_format_info *(*get_format_info)(u32 pixel_format, u64 modifier);";
        assert_eq!(header_fb_create_takes_format_info(new), Some(true));
        assert_eq!(header_fb_create_takes_format_info(old), Some(false));
        assert_eq!(header_fb_create_takes_format_info("nothing"), None);
        // A backport in a 6.12 distribution kernel.
        assert_eq!(
            kernel_fb_create_format_info("6.12.0", Some(new)),
            Some(true)
        );
        assert_eq!(
            kernel_fb_create_format_info("6.12.0", Some(old)),
            Some(false)
        );
        assert_eq!(kernel_fb_create_format_info("6.12.0", None), None);
        assert_eq!(kernel_fb_create_format_info("6.18.2", None), Some(true));
    }

    #[test]
    fn the_summary_names_every_fact_and_the_decision() {
        let host = wayland_host();
        let lines = summary_lines(&host, &plan_install_provider(&host));
        let text = lines.join("\n");
        for expected in [
            "Xorg server:      not installed",
            "gnome-shell 49.4 (headless, virtual monitors)",
            "systemd-logind yes, dbus-daemon yes",
            "1.4.11, WirePlumber yes, PulseAudio server /usr/bin/pipewire-pulse",
            "/dev/dri/renderD128 (GPU 0000:00:10.0, NVIDIA 0x10de), open to session users",
            "580.105.08, nvidia_drm loaded, modeset Y",
            "(DRM fb_create takes format info)",
            "auto (default)",
            "Decision:         Wayland",
        ] {
            assert!(text.contains(expected), "{expected:?} missing from\n{text}");
        }
        let xorg = xorg_host();
        let text = summary_lines(&xorg, &plan_install_provider(&xorg)).join("\n");
        assert!(
            text.contains("Xorg server:      /usr/libexec/Xorg"),
            "{text}"
        );
        assert!(text.contains("Decision:         X11"), "{text}");
        let mut none = wayland_host();
        none.render_nodes.clear();
        none.gpu_driver = None;
        let text = summary_lines(&none, &plan_install_provider(&none)).join("\n");
        assert!(text.contains("DRM render node:  none"), "{text}");
        assert!(text.contains("NVIDIA driver:    not loaded"), "{text}");
        assert!(
            text.contains("the installer stops (no_display_stack)"),
            "{text}"
        );
    }
}
