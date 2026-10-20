//! Arcen-managed headless GNOME/Mutter Wayland session (Linux host side).
//!
//! Process model, one logind session per desktop:
//!
//! ```text
//! Pier (root) ── session-launcher (root, PAM + pam_systemd, seatless
//!                │                  `XDG_SESSION_TYPE=wayland` logind session)
//!                └─ session-agent --wayland-headless (session user)
//!                     ├─ dbus-daemon --session   (private bus, private runtime dir)
//!                     ├─ pipewire + wireplumber  (video only; never ALSA/BlueZ)
//!                     └─ gnome-shell|mutter --headless --virtual-monitor WxH@R
//!                          └─ Xwayland :n (Mutter's own, unless `--no-x11`)
//! ```
//!
//! The agent owns the compositor stack as an [`arcen_outputs::OutputProvider`]
//! binding, then owns the Mutter `ScreenCast` and `RemoteDesktop` D-Bus
//! sessions for as long as the stream lives, and serves session-bound input
//! on a socket inside the private runtime directory. Readiness, output
//! verification and capability facts are decided by the pure
//! [`arcen_session::wayland_session`] policy; this module only gathers what
//! the operating system reports.
//!
//! The pure pieces here (argument shapes, ready messages, input commands and
//! held-input tracking) compile on every platform so they stay unit-tested on
//! the macOS development machine. Everything that spawns a process or talks
//! to D-Bus is Linux- and `wayland-provider`-only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use arcen_media::video::capture_handoff::{
    format_pci_address, is_render_node_path, parse_pci_address, parse_pci_vendor,
    CaptureHandoffError, PipeWireCaptureHandoff, ScreenCastCursorMode, NVIDIA_PCI_VENDOR,
    PIPEWIRE_NODE_ENV,
};
use arcen_media::video::PciAddress;
use arcen_session::compositor_prefix::{
    verify_prefix, CompositorPrefix, CompositorPrefixError, EntryKind, PathOwnership,
    PrefixInspector,
};
use arcen_session::desktop_provider::{
    select_desktop_provider, DesktopProviderSelection, HostDesktopFacts, ProviderPreference,
    ProviderRefusal,
};
use arcen_session::wayland_outputs::{PlacedOutput, VirtualOutputMode};
use arcen_session::wayland_qualification::QualifiedHost;
use arcen_session::wayland_session::{HeadlessCompositor, VirtualOutputRequest};
use arcen_session::wayland_x11::{
    is_mutter_xauthority, GlamorRoute, SessionDisplays, X11AppsPlan, X11AppsRefusal,
    X11AppsSetting, XWAYLAND_BINARY,
};
use serde::{Deserialize, Serialize};

#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod agent;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod audio;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod autostart;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod clipboard;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod eis;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod eis_text;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod facts;
pub mod input;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod keyring;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod mutter;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod output;
#[cfg(target_os = "linux")]
pub mod output_client;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod qualification;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod qualify;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod reconfigure;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod remote;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod session_manager;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod session_services;
#[cfg(target_os = "linux")]
pub mod socket_proof;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod stack;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod tablet_proof;
pub mod text;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod wl_keymap;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod xkb;
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub mod xwayland;

/// Whether this build carries the headless Wayland session adapter.
pub const WAYLAND_ADAPTER_BUILT: bool =
    cfg!(all(target_os = "linux", feature = "wayland-provider"));

/// Agent flag: the launcher writes the login password to the agent's
/// standard input (then closes it), for the session's keyring.
pub const AGENT_KEYRING_STDIN_FLAG: &str = "--keyring-stdin";
/// Agent and launcher flag carrying `wayland.first_login_setup`.
pub const FIRST_LOGIN_SETUP_FLAG: &str = "--first-login-setup";

/// Agent flag selecting the headless Wayland desktop.
pub const AGENT_WAYLAND_FLAG: &str = "--wayland-headless";
/// Agent flag naming the private runtime directory it must create.
pub const AGENT_RUNTIME_DIR_FLAG: &str = "--wayland-runtime-dir";
/// Agent flag naming one virtual output, `WIDTHxHEIGHT@HZ`.
pub const AGENT_OUTPUT_FLAG: &str = "--virtual-output";
/// Agent flag: start the compositor with its Xwayland, for X11 applications.
pub const AGENT_X11_FLAG: &str = "--x11-apps";
/// Launcher flag carrying the Pier's X11-applications decision.
pub const LAUNCHER_X11_FLAG: &str = "--wayland-x11-apps";
/// Agent flag naming the session's stream cursor mode
/// ([`StreamCursorMode`] token). Absent means embedded.
pub const AGENT_CURSOR_MODE_FLAG: &str = "--cursor-mode";
/// Agent flag naming the route to a wide composition the session starts
/// with (`force-hdr`), for a Grading or HDR session on a host whose
/// qualification proved it.
pub const AGENT_FIDELITY_ROUTE_FLAG: &str = "--fidelity-route";

/// Wayland socket name inside the private runtime directory.
pub const WAYLAND_DISPLAY_NAME: &str = "wayland-0";
/// Session-bound input socket inside the private runtime directory.
pub const INPUT_SOCKET_NAME: &str = "arcen-input.sock";
/// Session-bound output reconfiguration socket inside the private runtime
/// directory (remote-monitor sessions only).
pub const OUTPUT_SOCKET_NAME: &str = "arcen-output.sock";
/// Session-bound clipboard bridge socket inside the private runtime
/// directory, present only when the agent proved the compositor clipboard.
pub const CLIPBOARD_SOCKET_NAME: &str = "arcen-clipboard.sock";
/// Private D-Bus socket inside the private runtime directory.
pub const BUS_SOCKET_NAME: &str = "bus";
/// `PipeWire` socket inside the private runtime directory.
pub const PIPEWIRE_SOCKET_NAME: &str = "pipewire-0";

/// Output generation of a session's first output set. Each reconfiguration
/// (resize, topology change) mints the next one; a coordinate or capture of
/// any other generation is refused.
pub const INITIAL_OUTPUT_GENERATION: u64 = 1;

/// How every stream of a session reports the cursor. Chosen once when the
/// desktop opens (`arcen_media::video::capture_handoff::resolve_stream_cursor_mode`)
/// and kept for the desktop's life, through every output reconfiguration:
///
/// - `embedded`: Mutter draws the cursor into the frames (`cursor=host`).
///   The default, and what a Deck that cannot draw cursor images gets.
/// - `metadata`: Mutter reports the cursor as `SPA_META_Cursor`; capenc
///   sends its images to the Pier and the Deck draws it (`cursor=local`).
/// - `hidden`: neither drawn nor reported; the Deck draws its own pointer
///   without the host's shape (`cursor=local`). What a Deck that draws the
///   cursor gets where metadata is not proven or it cannot draw images.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamCursorMode {
    /// Drawn into the frames.
    #[default]
    Embedded,
    /// Reported as metadata.
    Metadata,
    /// Neither drawn nor reported.
    Hidden,
}

impl StreamCursorMode {
    /// The `ScreenCast` cursor mode.
    #[must_use]
    pub const fn screencast(self) -> ScreenCastCursorMode {
        match self {
            Self::Embedded => ScreenCastCursorMode::Embedded,
            Self::Metadata => ScreenCastCursorMode::Metadata,
            Self::Hidden => ScreenCastCursorMode::Hidden,
        }
    }

    /// The mode for a `ScreenCast` cursor mode.
    #[must_use]
    pub const fn from_screencast(mode: ScreenCastCursorMode) -> Option<Self> {
        match mode {
            ScreenCastCursorMode::Embedded => Some(Self::Embedded),
            ScreenCastCursorMode::Metadata => Some(Self::Metadata),
            ScreenCastCursorMode::Hidden => Some(Self::Hidden),
        }
    }

    /// Stable token (the agent flag's value).
    #[must_use]
    pub const fn token(self) -> &'static str {
        self.screencast().token()
    }

    /// The mode for a token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        ScreenCastCursorMode::from_token(token).and_then(Self::from_screencast)
    }
}

/// Default virtual output when the Deck named no display.
pub const DEFAULT_OUTPUT: (u32, u32, u32) = (1920, 1080, 60);

/// Notes a Deck display Speed was requested at without qualification
/// evidence, in the root-owned list beside `target`'s record
/// ([`arcen_session::wayland_qualification::RequestedSpeedOutputs`]): the
/// next `arcen-pier wayland qualify` measures it. Sizes only, never
/// evidence; written off the async runtime, and a failure is only logged.
pub fn note_unqualified_speed_display(
    target: &arcen_session::wayland_qualification::QualificationTarget,
    width: u32,
    height: u32,
    display_refresh_hz: u32,
    codec: &str,
) {
    #[cfg(all(target_os = "linux", feature = "wayland-provider"))]
    {
        let Some(output) = arcen_session::wayland_qualification::QualifyOutput::new(
            width,
            height,
            display_refresh_hz,
        ) else {
            return;
        };
        let path = std::path::PathBuf::from(target.requested_outputs_path());
        let command = target.qualify_command();
        let codec = codec.to_owned();
        tokio::task::spawn_blocking(move || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            let mut noted = false;
            match qualification::update_requested_outputs(&path, |list| {
                noted = list.note(output, &codec, now);
                noted
            }) {
                Ok(_) if noted => tracing::info!(
                    target: crate::logging::target::SESSION,
                    display = %output,
                    codec,
                    path = %path.display(),
                    "Speed was requested at a display this host has not qualified; it is served \
                     as Auto and recorded for the next `{command}`"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    target: crate::logging::target::SESSION,
                    display = %output,
                    %error,
                    path = %path.display(),
                    "the unqualified Speed display could not be recorded"
                ),
            }
        });
    }
    #[cfg(not(all(target_os = "linux", feature = "wayland-provider")))]
    let _ = (target, width, height, display_refresh_hz, codec);
}

/// The agent's whole start — stack, output verification, D-Bus sessions,
/// first stream and input probe — must finish within this, or it releases
/// everything it started and fails. A healthy GNOME 40 start takes about
/// two seconds.
pub const WAYLAND_START_BUDGET: Duration = Duration::from_secs(40);
/// How long the launcher waits for the agent's ready message.
pub const WAYLAND_AGENT_READY_TIMEOUT: Duration = Duration::from_secs(45);
/// How long the Pier waits for the launcher's ready response: the agent's
/// ready timeout plus the launcher's logind checks (each bounded at 2 s)
/// and its own validation.
pub const WAYLAND_OPEN_TIMEOUT: Duration = Duration::from_secs(60);

const _: () = assert!(
    WAYLAND_START_BUDGET.as_secs() + 5 <= WAYLAND_AGENT_READY_TIMEOUT.as_secs()
        && WAYLAND_AGENT_READY_TIMEOUT.as_secs() + 10 <= WAYLAND_OPEN_TIMEOUT.as_secs()
);

/// The render node a compositor holds open, from the targets of its
/// `/proc/<pid>/fd` links: the one `/dev/dri/renderD<minor>` among them, or
/// none when it holds none or several distinct ones.
#[must_use]
pub fn single_render_node<'a>(fd_targets: impl IntoIterator<Item = &'a Path>) -> Option<PathBuf> {
    let nodes = fd_targets
        .into_iter()
        .filter(|target| is_render_node_path(target))
        .map(Path::to_path_buf)
        .collect::<BTreeSet<_>>();
    (nodes.len() == 1)
        .then(|| nodes.into_iter().next())
        .flatten()
}

/// The PCI device (`0000:00:10.0`) and vendor id of a render node, from its
/// sysfs class entry below `sys_root` (normally `/sys`). Each fact is `None`
/// when sysfs does not state it.
#[must_use]
pub fn render_node_gpu(sys_root: &Path, render_node: &Path) -> (Option<String>, Option<u16>) {
    let Some(name) = render_node.file_name() else {
        return (None, None);
    };
    let device = sys_root.join("class/drm").join(name).join("device");
    let pci = std::fs::canonicalize(&device)
        .ok()
        .and_then(|path| parse_pci_address(path.file_name()?.to_str()?))
        .map(format_pci_address);
    let vendor = std::fs::read_to_string(device.join("vendor"))
        .ok()
        .as_deref()
        .and_then(parse_pci_vendor);
    (pci, vendor)
}

/// The encoder GPU a Wayland session names for capenc: the compositor's own
/// GPU, and only when it is proven to be an NVIDIA device, which capenc's
/// native encoder can open by PCI address. Any other compositor GPU (an
/// integrated or emulated adapter next to the NVIDIA GPU) names no encoder,
/// so capenc keeps its own device choice and its cross-GPU route.
#[must_use]
pub fn session_encoder_gpu(
    compositor_gpu: Option<PciAddress>,
    compositor_vendor: Option<u16>,
) -> Option<PciAddress> {
    compositor_gpu.filter(|_| compositor_vendor == Some(NVIDIA_PCI_VENDOR))
}

/// What probing one compositor binary established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The binary is not installed.
    NotInstalled,
    /// It is installed but `--help` failed or timed out.
    Failed,
    /// It was probed.
    Probed(arcen_session::wayland_session::CompositorProbe),
}

/// Why no compositor was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorChoiceError {
    /// The compositor that would have been chosen could not be probed.
    ProbeFailed(HeadlessCompositor),
    /// No installed compositor offers headless mode with virtual monitors.
    NoneUsable,
}

/// Walks probe outcomes in preference order: the first usable compositor is
/// chosen; a failed probe before it stops the choice (it might have been
/// usable); a failed probe after it is irrelevant.
///
/// # Errors
///
/// [`CompositorChoiceError`].
pub fn choose_from_outcomes(
    outcomes: &[(HeadlessCompositor, ProbeOutcome)],
) -> Result<HeadlessCompositor, CompositorChoiceError> {
    for (compositor, outcome) in outcomes {
        match outcome {
            ProbeOutcome::NotInstalled => {}
            ProbeOutcome::Failed => return Err(CompositorChoiceError::ProbeFailed(*compositor)),
            ProbeOutcome::Probed(probe) if probe.usable() => return Ok(*compositor),
            ProbeOutcome::Probed(_) => {}
        }
    }
    Err(CompositorChoiceError::NoneUsable)
}

/// The cgroup v2 directory a `/proc/<pid>/cgroup` text names, under
/// `/sys/fs/cgroup`. `None` without a unified (`0::`) entry or with a path
/// that is not absolute or contains `..`.
#[must_use]
pub fn unified_cgroup(cgroup: &str) -> Option<PathBuf> {
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let relative = Path::new(path).strip_prefix("/").ok()?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(Path::new("/sys/fs/cgroup").join(relative))
}

/// FUSE mount points owned by `uid` at or below `dir`, deepest first, from
/// `/proc/self/mountinfo` text.
///
/// Teardown only ever unmounts these, and only as the session user through
/// `fusermount`, which itself refuses a mount the caller does not own.
/// Anything else mounted under a user-writable directory is never touched.
#[must_use]
pub fn user_fuse_mounts_inside(mountinfo: &str, dir: &Path, uid: u32) -> Vec<PathBuf> {
    let owner = format!("user_id={uid}");
    let mut mounts = mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let mount_point = unescape_mountinfo(before.split(' ').nth(4)?)?;
            let mut after = after.split(' ');
            let fstype = after.next()?;
            let super_options = after.nth(1)?;
            let fuse = fstype == "fuse" || fstype.starts_with("fuse.");
            let owned = super_options.split(',').any(|option| option == owner);
            (fuse && owned && mount_point.starts_with(dir)).then_some(mount_point)
        })
        .collect::<Vec<_>>();
    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.components().count()));
    mounts
}

/// Decodes mountinfo's octal escapes (`\040` space, `\011` tab, `\012`
/// newline, `\134` backslash). Malformed escapes refuse the entry.
fn unescape_mountinfo(field: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let digits = bytes.get(index + 1..index + 4)?;
            if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                return None;
            }
            let value = digits
                .iter()
                .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
            decoded.push(u8::try_from(value).ok()?);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(decoded));
    path.is_absolute().then_some(path)
}

/// The private runtime directory of one logind session.
///
/// It lives under the user's logind runtime directory, so logind removes it
/// with the user's last session even if Arcen could not.
#[must_use]
pub fn private_runtime_dir(uid: u32, session_id: &str) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/arcen-wayland-{session_id}"))
}

/// Whether `path` is exactly the private runtime directory Arcen would use
/// for this user and session.
#[must_use]
pub fn is_private_runtime_dir(path: &Path, uid: u32, session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && path == private_runtime_dir(uid, session_id)
}

/// Formats a virtual output for `--virtual-monitor` and the agent flag.
#[must_use]
pub fn format_output(output: &VirtualOutputRequest) -> String {
    format!(
        "{}x{}@{}",
        output.width(),
        output.height(),
        output.refresh_hz()
    )
}

/// `WIDTHxHEIGHT@HZ+X+Y`: an output and its place in the desktop, followed
/// by `~WMMxHMM~LOGICAL` when the client display's density is known.
#[must_use]
pub fn format_placed_output(output: &PlacedOutput) -> String {
    let placed = format!(
        "{}+{}+{}",
        format_output(&output.request),
        output.x,
        output.y
    );
    if output.density.is_known() {
        format!(
            "{placed}~{}x{}~{}",
            output.density.width_mm, output.density.height_mm, output.density.logical_width
        )
    } else {
        placed
    }
}

/// Parses `WIDTHxHEIGHT@HZ`, optionally followed by `+X+Y` (non-negative)
/// and `~WMMxHMM~LOGICAL`; without a position the output is at the origin.
#[must_use]
pub fn parse_placed_output(value: &str) -> Option<PlacedOutput> {
    let (value, density) = match value.split_once('~') {
        None => (
            value,
            arcen_session::wayland_outputs::OutputDensity::default(),
        ),
        Some((value, density)) => {
            let (size, logical) = density.split_once('~')?;
            let (width_mm, height_mm) = size.split_once('x')?;
            let number = |text: &str| -> Option<u32> {
                (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
                    .then(|| text.parse().ok())
                    .flatten()
            };
            (
                value,
                arcen_session::wayland_outputs::OutputDensity {
                    width_mm: number(width_mm)?,
                    height_mm: number(height_mm)?,
                    logical_width: number(logical)?,
                },
            )
        }
    };
    parse_position(value).map(|placed| placed.with_density(density))
}

fn parse_position(value: &str) -> Option<PlacedOutput> {
    let mut parts = value.split('+');
    let request = parse_output(parts.next()?)?;
    let coordinate = |text: Option<&str>| -> Option<i32> {
        let text = text?;
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    match (parts.next(), parts.next(), parts.next()) {
        (None, None, None) => Some(PlacedOutput::at_origin(request)),
        (x @ Some(_), y @ Some(_), None) => Some(PlacedOutput {
            request,
            x: coordinate(x)?,
            y: coordinate(y)?,
            density: arcen_session::wayland_outputs::OutputDensity::default(),
        }),
        _ => None,
    }
}

/// Parses `WIDTHxHEIGHT@HZ`.
#[must_use]
pub fn parse_output(value: &str) -> Option<VirtualOutputRequest> {
    let (size, refresh) = value.split_once('@')?;
    let (width, height) = size.split_once('x')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(width) || !digits(height) || !digits(refresh) {
        return None;
    }
    VirtualOutputRequest::new(
        width.parse().ok()?,
        height.parse().ok()?,
        refresh.parse().ok()?,
    )
    .ok()
}

/// The prefix this process runs the compositor from, set once at start
/// (`capture.wayland_compositor_prefix` in the Pier and `wayland qualify`,
/// the launcher's open request in the agent). Unset is `/usr`.
static COMPOSITOR_PREFIX: std::sync::OnceLock<CompositorPrefix> = std::sync::OnceLock::new();

/// Flag carrying the compositor prefix from the launcher to the agent.
pub const AGENT_COMPOSITOR_PREFIX_FLAG: &str = "--compositor-prefix";

/// Flag carrying the lab seat pen's udev seat from the launcher to the
/// agent (`wayland.lab_seat_pen`).
pub const AGENT_LAB_INPUT_SEAT_FLAG: &str = "--lab-input-seat";

/// The udev seat a lab-patched compositor runs libinput on, set once in the
/// agent from [`AGENT_LAB_INPUT_SEAT_FLAG`]; unset, the compositor opens no
/// input devices.
static LAB_INPUT_SEAT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Flag telling the agent that its desktop's compositor runs natively on
/// [`AGENT_LAB_INPUT_SEAT_FLAG`]'s seat (`wayland.session_mode`).
pub const AGENT_NATIVE_SEAT_FLAG: &str = "--native-seat";

/// Whether this agent's compositor runs natively on its desktop's seat.
static NATIVE_SEAT: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Marks this agent's compositor as native on its desktop's seat.
pub fn set_native_seat() {
    let _ = NATIVE_SEAT.set(());
}

/// Whether this agent's compositor runs natively on its desktop's seat.
#[must_use]
pub fn native_seat() -> bool {
    NATIVE_SEAT.get().is_some()
}

/// [`compositor_arguments`] for a compositor that runs natively on its
/// logind session's seat: a full display server (`--display-server`)
/// instead of `--headless`, so stock Mutter keeps libinput and opens the
/// seat's devices through logind. Everything else is unchanged.
#[must_use]
pub fn native_seat_arguments(arguments: Vec<String>) -> Vec<String> {
    arguments
        .into_iter()
        .map(|argument| {
            if argument == "--headless" {
                "--display-server".to_owned()
            } else {
                argument
            }
        })
        .collect()
}

/// Sets the lab input seat (first value wins).
pub fn set_lab_input_seat(seat: &str) {
    let _ = LAB_INPUT_SEAT.set(seat.to_owned());
}

/// The lab input seat, when this agent's desktop has one.
#[must_use]
pub fn lab_input_seat() -> Option<&'static str> {
    LAB_INPUT_SEAT.get().map(String::as_str)
}

/// Sets the prefix this process runs the compositor from. A second call
/// with the same prefix is a no-op.
///
/// # Errors
///
/// When another prefix was already set: one process never mixes two
/// compositor installations.
pub fn set_compositor_prefix(prefix: CompositorPrefix) -> Result<(), String> {
    let current = COMPOSITOR_PREFIX.get_or_init(|| prefix.clone());
    if *current == prefix {
        Ok(())
    } else {
        Err(format!(
            "the compositor prefix is already {current}, not {prefix}"
        ))
    }
}

/// The prefix this process runs the compositor from (`/usr` unless set).
#[must_use]
pub fn compositor_prefix() -> &'static CompositorPrefix {
    static SYSTEM: std::sync::OnceLock<CompositorPrefix> = std::sync::OnceLock::new();
    COMPOSITOR_PREFIX
        .get()
        .unwrap_or_else(|| SYSTEM.get_or_init(CompositorPrefix::system))
}

/// The compositor binary for `compositor`, in this process's prefix.
#[must_use]
pub fn compositor_binary(compositor: HeadlessCompositor) -> String {
    compositor_prefix().binary(compositor)
}

/// Checks that a root process may execute anything from `prefix` (shared
/// `verify_prefix`: both the configured and the real path, and every file,
/// directory and link inside, root-owned and writable by no one else) and
/// returns the compositors it holds. `/usr` is the distribution's and is not
/// checked.
///
/// # Errors
///
/// The first path that fails, or a prefix without a compositor.
pub fn verify_compositor_prefix(
    prefix: &CompositorPrefix,
) -> Result<Vec<HeadlessCompositor>, CompositorPrefixError> {
    verify_prefix(prefix, &HostPrefixInspector)
}

/// [`PrefixInspector`] over this host's filesystem.
struct HostPrefixInspector;

impl PrefixInspector for HostPrefixInspector {
    fn inspect(&self, path: &str) -> PathOwnership {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => PathOwnership {
                path: path.to_owned(),
                uid: Some(metadata.uid()),
                mode: metadata.mode() & 0o7777,
                kind: if metadata.file_type().is_symlink() {
                    EntryKind::Symlink
                } else if metadata.is_dir() {
                    EntryKind::Directory
                } else if metadata.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Other
                },
            },
            Err(_) => PathOwnership {
                path: path.to_owned(),
                uid: None,
                mode: 0,
                kind: EntryKind::Other,
            },
        }
    }

    fn children(&self, path: &str) -> Option<Vec<String>> {
        std::fs::read_dir(path)
            .ok()?
            .map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect()
    }

    fn resolve(&self, path: &str) -> Option<String> {
        std::fs::canonicalize(path)
            .ok()?
            .into_os_string()
            .into_string()
            .ok()
    }
}

/// Runs a `--help`/`--version` probe of a side-by-side prefix's binary as
/// `nobody` when this process is root: the prefix passed
/// [`verify_compositor_prefix`], and a probe still never needs root.
/// `/usr` probes keep running as they always did.
pub(crate) fn unprivileged_probe(command: &mut tokio::process::Command, binary: &str) {
    if probe_as_nobody(
        compositor_prefix(),
        binary,
        nix::unistd::geteuid().is_root(),
    ) {
        command.uid(NOBODY).gid(NOBODY).env("HOME", "/");
    }
}

fn probe_as_nobody(prefix: &CompositorPrefix, binary: &str, root: bool) -> bool {
    root && !prefix.is_system() && binary.starts_with(&format!("{}/", prefix.as_str()))
}

/// `nobody` / `nogroup` (the kernel's overflow id).
const NOBODY: u32 = 65_534;

/// Parses, verifies and sets this process's compositor prefix from the
/// Pier configuration's `capture.wayland_compositor_prefix`; `None` keeps
/// `/usr`. Every compositor in the prefix is verified before any is probed
/// or run.
///
/// # Errors
///
/// A malformed prefix, one a root process may not execute from, or a
/// second, different prefix in the same process.
pub fn apply_configured_compositor_prefix(configured: Option<&str>) -> Result<(), String> {
    let Some(value) = configured else {
        return Ok(());
    };
    let prefix = CompositorPrefix::parse(value)
        .map_err(|error| format!("capture.wayland_compositor_prefix: {error}"))?;
    verify_compositor_prefix(&prefix)
        .map_err(|error| format!("capture.wayland_compositor_prefix {prefix}: {error}"))?;
    set_compositor_prefix(prefix)
}

/// Arguments for a headless compositor with exactly these virtual outputs.
/// A remote-monitor session starts with none: its outputs are the
/// `ScreenCast` session's `RecordVirtual` monitors.
///
/// With `x11` the compositor runs its Xwayland for X11 applications. A
/// compositor in a logind session scope (not a systemd user unit) treats it
/// as mandatory: Mutter starts it at once and exits if it dies.
#[must_use]
pub fn compositor_arguments(
    outputs: &[VirtualOutputRequest],
    mode: VirtualOutputMode,
    x11: bool,
) -> Vec<String> {
    let outputs = match mode {
        VirtualOutputMode::StartupMonitors => outputs,
        VirtualOutputMode::RemoteMonitors => &[],
    };
    let mut arguments = vec!["--headless".to_owned(), "--wayland".to_owned()];
    if !x11 {
        arguments.push("--no-x11".to_owned());
    }
    arguments.extend([
        "--wayland-display".to_owned(),
        WAYLAND_DISPLAY_NAME.to_owned(),
    ]);
    for output in outputs {
        arguments.push("--virtual-monitor".to_owned());
        arguments.push(format_output(output));
    }
    arguments
}

pub use arcen_session::wayland_session::parse_compositor_help;

/// `arcen-pier wayland …`: host qualification for the headless Wayland
/// provider (`wayland qualify`). A build without `wayland-provider` has none.
#[must_use]
pub fn wayland_main(args: &[String]) -> std::process::ExitCode {
    #[cfg(all(target_os = "linux", feature = "wayland-provider"))]
    {
        qualify::main(args)
    }
    #[cfg(not(all(target_os = "linux", feature = "wayland-provider")))]
    {
        let _ = args;
        eprintln!("the headless Wayland provider is not available in this build");
        std::process::ExitCode::from(2)
    }
}

/// Whether an Xorg server executable is installed: the configured
/// `platform.session.xorg_bin`, or any well-known location.
#[must_use]
pub fn xorg_server_installed(configured: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::iter::once(configured)
        .chain(
            arcen_session::desktop_provider::WELL_KNOWN_XORG_SERVERS
                .iter()
                .map(Path::new),
        )
        .any(|path| {
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

/// Whether the Xwayland executable the compositor runs is installed.
#[must_use]
pub fn xwayland_installed() -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(XWAYLAND_BINARY)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Decides at start whether this Pier's Wayland sessions run Xwayland, from
/// `platform.session.wayland_x11_apps`, the installed Xwayland and the
/// host's qualification record, and says why.
///
/// # Errors
///
/// [`X11AppsRefusal`] when the operator set `on` and it cannot be honoured.
pub fn resolve_configured_x11_apps(
    setting: X11AppsSetting,
    qualification: Option<&QualifiedHost>,
) -> Result<X11AppsPlan, X11AppsRefusal> {
    let installed = xwayland_installed();
    let proof = qualification.and_then(|qualified| qualified.record().facts.x11_apps.as_ref());
    let result = arcen_session::wayland_x11::resolve_x11_apps(setting, installed, proof);
    match &result {
        Ok(plan) => tracing::info!(
            target: crate::logging::target::SESSION,
            setting = setting.token(),
            x11_apps = plan.token(),
            xwayland_installed = installed,
            glamor = proof.map(|proof| proof.glamor.token()),
            xwayland_version = proof.and_then(|proof| proof.xwayland_version.as_deref()),
            "X11 applications in Wayland sessions: {}",
            match plan {
                X11AppsPlan::Enabled => "served through the compositor's Xwayland".to_owned(),
                X11AppsPlan::Disabled(reason) => format!("not served ({}): {}", reason.token(), reason.remedy()),
            }
        ),
        Err(refusal) => tracing::error!(
            target: crate::logging::target::SESSION,
            setting = setting.token(),
            reason = refusal.reason.token(),
            "{refusal}"
        ),
    }
    result
}

/// The X11-application plan of a Pier whose provider is settled: `None`
/// unless it serves Wayland sessions, otherwise
/// [`resolve_configured_x11_apps`] on the record the provider was selected
/// with.
///
/// # Errors
///
/// [`X11AppsRefusal`] when the operator set `on` and it cannot be honoured.
pub fn configured_x11_plan(
    provider: arcen_session::desktop_provider::DesktopProvider,
    setting: X11AppsSetting,
    qualification: Option<&QualifiedHost>,
) -> Result<Option<X11AppsPlan>, X11AppsRefusal> {
    if provider != arcen_session::desktop_provider::DesktopProvider::Wayland {
        return Ok(None);
    }
    resolve_configured_x11_apps(setting, qualification).map(Some)
}

/// The provider a Pier runs and, for Wayland, the qualification record that
/// supplied its session-proven host facts.
#[derive(Debug, Clone)]
pub struct ConfiguredProvider {
    /// What selection decided.
    pub selection: DesktopProviderSelection,
    /// The validated record, for Wayland.
    pub qualification: Option<std::sync::Arc<QualifiedHost>>,
}

/// Selects the Linux Pier's desktop provider at start, from host facts.
///
/// `auto` (the default): an installed Xorg server keeps the dedicated Xorg
/// session, without probing anything Wayland, so an Xorg installation is
/// never moved. Without one, headless Wayland is selected when the live
/// probe (compositor, logind, D-Bus, PipeWire, a DRM render node) and the
/// host's qualification record (`capture.wayland_qualification_record`,
/// default [`qualification::RECORD_PATH`], written by
/// `arcen-pier wayland qualify --write` for exactly this Pier build,
/// compositor, PipeWire and GPU driver) together prove every fact. A host
/// with neither Xorg nor DRM is refused with the driver to install; Arcen
/// never changes driver configuration. `xorg`/`wayland` override the choice
/// and refuse to start when they cannot be honoured.
///
/// # Errors
///
/// [`ProviderRefusal`] naming what is missing; never the other provider.
pub async fn select_configured_provider(
    preference: ProviderPreference,
    xorg_bin: &Path,
    record: &Path,
) -> Result<ConfiguredProvider, ProviderRefusal> {
    let xorg_server = xorg_server_installed(xorg_bin);
    let needs_wayland = match preference {
        ProviderPreference::Auto => !xorg_server,
        ProviderPreference::Xorg => false,
        ProviderPreference::Wayland => true,
    };
    let (facts, qualification) = if needs_wayland {
        wayland_facts(xorg_server, record).await
    } else {
        (
            HostDesktopFacts {
                xorg_server,
                wayland_built: WAYLAND_ADAPTER_BUILT,
                ..HostDesktopFacts::default()
            },
            None,
        )
    };
    let result = select_desktop_provider(preference, &facts).map(|selection| ConfiguredProvider {
        selection,
        qualification: qualification.map(std::sync::Arc::new),
    });
    match &result {
        Ok(configured) => {
            let record_path = record;
            let record = configured
                .qualification
                .as_ref()
                .map(|qualified| qualified.record());
            tracing::info!(
                target: crate::logging::target::SESSION,
                preference = preference.token(),
                provider = configured.selection.provider.token(),
                reason = configured.selection.reason.token(),
                xorg_server,
                record_path = needs_wayland.then(|| record_path.display().to_string()),
                qualified_at_unix_s = record.map(|record| record.qualified_at_unix_s),
                capture_route = record
                    .and_then(|record| record.facts.capture_route.as_ref())
                    .map(|proof| proof.route.token()),
                source_rates = ?record.map(|record| &record.facts.source_rates),
                "desktop provider selected"
            );
        }
        Err(refusal) => tracing::error!(
            target: crate::logging::target::SESSION,
            preference = preference.token(),
            xorg_server,
            blockers = ?refusal.tokens(),
            "{refusal}"
        ),
    }
    result
}

/// What start-up did about a refused Wayland record.
#[derive(Debug)]
pub enum StartRequalify {
    /// Not a case for automatic re-qualification: the refusal stands.
    NotApplicable,
    /// Re-qualified; the provider selected with the new record.
    Selected(ConfiguredProvider),
    /// Re-qualification failed now, or failed recently on this same stack.
    Refused(String),
    /// A stop was requested during the run: its desktop and temporary
    /// account are gone, and no attempt was recorded.
    Interrupted,
}

/// After provider selection refused Wayland, re-qualifies the host when an
/// update made its record stale
/// (`arcen_session::wayland_qualification::auto_requalify_decision`), then
/// selects again. Bounded, logged step by step, and recorded in an attempt
/// marker beside the record so a failure is not retried on every restart.
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
pub async fn requalify_at_start(
    config: &crate::cli::Config,
    refusal: &ProviderRefusal,
    stop: tokio::sync::watch::Receiver<crate::service_signals::StopRequest>,
) -> StartRequalify {
    use arcen_session::wayland_qualification::{
        auto_requalify_decision, identity_digest, AutoRequalify, RequalifyAttempt,
        REQUALIFY_RETRY_AFTER_S,
    };
    let target = &config.wayland_qualification_target;
    let marker_path = std::path::PathBuf::from(target.requalify_marker_path());
    let identity = match facts::probe_host().await.compositor() {
        Some(compositor) => qualification::host_identity(compositor).await.ok(),
        None => None,
    };
    let last = qualification::read_root_only(&marker_path)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<RequalifyAttempt>(&text).ok());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    match auto_requalify_decision(
        refusal,
        config.wayland_auto_requalify,
        identity.as_ref(),
        last.as_ref(),
        now,
    ) {
        AutoRequalify::NotApplicable => StartRequalify::NotApplicable,
        AutoRequalify::RecentFailure {
            attempted_at_unix_s,
            failures,
        } => StartRequalify::Refused(format!(
            "{refusal}; automatic re-qualification on this same stack failed {} min ago ({}) \
             and is not retried for {} h; fix it and run `{}` as root",
            now.saturating_sub(attempted_at_unix_s) / 60,
            failures.join("; "),
            REQUALIFY_RETRY_AFTER_S / 3600,
            target.qualify_command(),
        )),
        AutoRequalify::Run {
            reasons,
            interrupted_before,
        } => {
            if interrupted_before > 0 {
                tracing::warn!(
                    target: crate::logging::target::SESSION,
                    interrupted_before,
                    "the last automatic re-qualification on this stack ended without an outcome \
                     (the Pier was killed or crashed); cleaning up after it and trying again"
                );
            }
            tracing::warn!(
                target: crate::logging::target::SESSION,
                record = target.record_path(),
                %reasons,
                "the Wayland qualification record no longer describes this host (an update); \
                 re-qualifying automatically before serving (a short headless session as {})",
                target.temporary_user()
            );
            let started = std::time::Instant::now();
            // Written first: a start that still finds it running knows this
            // attempt was interrupted, not failed.
            let running = RequalifyAttempt::started(
                identity.as_ref().map(identity_digest).unwrap_or_default(),
                now,
                interrupted_before,
            );
            if let Err(error) = qualification::write_requalify_marker(&marker_path, &running) {
                tracing::warn!(
                    target: crate::logging::target::SESSION,
                    %error,
                    path = %marker_path.display(),
                    "the automatic re-qualification attempt could not be recorded as started"
                );
            }
            let outcome = qualify::run_automatic(config.clone(), target.clone(), stop).await;
            if outcome.interrupted {
                // An orderly stop is not an attempt.
                match std::fs::remove_file(&marker_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => tracing::warn!(
                        target: crate::logging::target::SESSION,
                        %error,
                        path = %marker_path.display(),
                        "the interrupted re-qualification's marker could not be removed"
                    ),
                }
                return StartRequalify::Interrupted;
            }
            let marker = running.finished(outcome.passed, outcome.failures.clone());
            if let Err(error) = qualification::write_requalify_marker(&marker_path, &marker) {
                tracing::warn!(
                    target: crate::logging::target::SESSION,
                    %error,
                    path = %marker_path.display(),
                    "the automatic re-qualification attempt could not be recorded"
                );
            }
            if !outcome.passed {
                tracing::error!(
                    target: crate::logging::target::SESSION,
                    seconds = started.elapsed().as_secs(),
                    failures = ?outcome.failures,
                    "automatic Wayland re-qualification failed"
                );
                return StartRequalify::Refused(format!(
                    "{refusal}; automatic re-qualification failed: {}; fix it and run `{}` as root",
                    outcome.failures.join("; "),
                    target.qualify_command(),
                ));
            }
            tracing::info!(
                target: crate::logging::target::SESSION,
                seconds = started.elapsed().as_secs(),
                record = target.record_path(),
                "automatic Wayland re-qualification passed; record written"
            );
            match select_configured_provider(
                config.provider_preference,
                &config.xorg_bin,
                Path::new(target.record_path()),
            )
            .await
            {
                Ok(configured) => StartRequalify::Selected(configured),
                Err(refused) => StartRequalify::Refused(refused.to_string()),
            }
        }
    }
}

/// Without the Wayland provider there is nothing to re-qualify.
#[cfg(not(all(target_os = "linux", feature = "wayland-provider")))]
pub async fn requalify_at_start(
    _config: &crate::cli::Config,
    _refusal: &ProviderRefusal,
    _stop: tokio::sync::watch::Receiver<crate::service_signals::StopRequest>,
) -> StartRequalify {
    StartRequalify::NotApplicable
}

/// Live Wayland facts with the session-proven ones from a valid record.
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
async fn wayland_facts(
    xorg_server: bool,
    record: &Path,
) -> (HostDesktopFacts, Option<QualifiedHost>) {
    let probe = facts::probe_host().await;
    let live = probe.facts();
    tracing::info!(
        target: crate::logging::target::SESSION,
        compositor = ?probe.compositor().map(HeadlessCompositor::token),
        facts = ?live,
        "headless Wayland provider facts gathered"
    );
    let validated = match probe.compositor() {
        // Without a compositor the blockers already say why.
        None => Err(None),
        Some(compositor) => match qualification::host_identity(compositor).await {
            Err(reason) => Err(Some(
                arcen_session::wayland_qualification::QualificationError::Unreadable {
                    path: record.display().to_string(),
                    reason: format!("the running Wayland stack could not be identified: {reason}"),
                },
            )),
            Ok(identity) => qualification::load_record(record)
                .and_then(|record| record.validate(&identity))
                .map_err(Some),
        },
    };
    let (wayland, qualification, qualified) = match validated {
        Ok(qualified) => (qualified.apply(live), None, Some(qualified)),
        Err(error) => (live, error, None),
    };
    (
        HostDesktopFacts {
            xorg_server,
            wayland_built: true,
            wayland,
            qualification,
        },
        qualified,
    )
}

#[cfg(not(all(target_os = "linux", feature = "wayland-provider")))]
async fn wayland_facts(
    xorg_server: bool,
    _record: &Path,
) -> (HostDesktopFacts, Option<QualifiedHost>) {
    (
        HostDesktopFacts {
            xorg_server,
            wayland_built: false,
            ..HostDesktopFacts::default()
        },
        None,
    )
}

/// One verified output in the agent's ready message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyOutput {
    /// Compositor connector.
    pub connector: String,
    /// Applied width.
    pub width: u32,
    /// Applied height.
    pub height: u32,
    /// Applied refresh in hertz.
    pub refresh_hz: u32,
}

/// One `ScreenCast` stream in the agent's ready message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyStream {
    /// Output connector it records.
    pub connector: String,
    /// `PipeWire` node id.
    pub node_id: u32,
    /// Stream width.
    pub width: u32,
    /// Stream height.
    pub height: u32,
    /// Output logical origin x.
    pub x: i32,
    /// Output logical origin y.
    pub y: i32,
}

/// The X11 part of the agent's ready message: whether the compositor's
/// Xwayland serves X11 clients, and the facts that prove it.
///
/// The Pier never puts these into its own session environment: Mutter
/// exports `DISPLAY`/`XAUTHORITY` to the applications it launches and to the
/// session bus's activation environment, and every X11-only Pier path
/// (cursor watcher, deskside, X capture, the X11 clipboard child) stays off
/// in a Wayland session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyX11 {
    /// An X11 client authenticated with the session cookie, was served, and
    /// found the screen managed.
    pub ready: bool,
    /// Why it is not ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// The displays the compositor locked, when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displays: Option<SessionDisplays>,
    /// Mutter's authority file, inside the private runtime directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xauthority: Option<PathBuf>,
    /// Where Xwayland's glamor renders.
    pub glamor: GlamorRoute,
}

impl ReadyX11 {
    /// Not served, for `failure`, with whatever was found.
    #[must_use]
    pub fn failed(failure: impl Into<String>, displays: Option<SessionDisplays>) -> Self {
        Self {
            ready: false,
            failure: Some(failure.into()),
            displays,
            xauthority: None,
            glamor: GlamorRoute::Unknown,
        }
    }

    /// `DISPLAY` for X11 applications (`:n`).
    #[must_use]
    pub fn display(&self) -> Option<String> {
        self.displays
            .map(|displays| arcen_session::wayland_x11::display_name(displays.public))
    }

    /// `GNOME_SETUP_DISPLAY`, the display Mutter keeps for managed services
    /// such as `gsd-xsettings`.
    #[must_use]
    pub fn setup_display(&self) -> Option<String> {
        self.displays
            .map(|displays| arcen_session::wayland_x11::display_name(displays.private))
    }

    /// Whether the message is internally consistent for `runtime_dir`.
    #[must_use]
    pub fn is_consistent(&self, runtime_dir: &Path) -> bool {
        let displays_ok = self.displays.is_none_or(|displays| {
            displays.public < displays.private
                && displays.private <= arcen_session::wayland_x11::MAX_X11_DISPLAY
                && displays.lock_pid != 0
        });
        let auth_ok = self
            .xauthority
            .as_ref()
            .is_none_or(|auth| is_mutter_xauthority(auth, runtime_dir));
        let ready_ok = !self.ready
            || (self.displays.is_some() && self.xauthority.is_some() && self.failure.is_none());
        displays_ok && auth_ok && ready_ok
    }
}

/// The Wayland part of the agent's ready message, reported to the launcher
/// and onwards to the Pier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaylandAgentReady {
    /// Compositor token (`gnome-shell` or `mutter`).
    pub compositor: String,
    /// Private runtime directory.
    pub runtime_dir: PathBuf,
    /// `WAYLAND_DISPLAY` inside it.
    pub wayland_display: String,
    /// Verified outputs.
    pub outputs: Vec<ReadyOutput>,
    /// One stream per output.
    pub streams: Vec<ReadyStream>,
    /// `RemoteDesktop` `SupportedDeviceTypes`.
    pub input_device_types: u32,
    /// Session-bound input socket.
    pub input_socket: PathBuf,
    /// Output generation the streams belong to.
    pub generation: u64,
    /// The DRM render node the compositor holds open, when exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compositor_render_node: Option<PathBuf>,
    /// That render node's PCI device, `0000:00:10.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compositor_gpu_pci: Option<String>,
    /// That device's PCI vendor id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compositor_gpu_vendor: Option<u16>,
    /// How the compositor provides outputs; a fixed set unless it is a
    /// remote-monitor session.
    #[serde(default = "startup_monitors")]
    pub output_mode: VirtualOutputMode,
    /// Session-bound output reconfiguration socket, when outputs can change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_socket: Option<PathBuf>,
    /// X11 applications, when the session was started with Xwayland.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x11: Option<ReadyX11>,
    /// How every stream of this session reports the cursor.
    #[serde(default)]
    pub cursor_mode: StreamCursorMode,
    /// The compositor clipboard the agent proved at start, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<WaylandClipboardReady>,
}

/// A compositor clipboard the session agent proved and serves on
/// [`CLIPBOARD_SOCKET_NAME`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaylandClipboardReady {
    /// The bridge socket inside the runtime directory.
    pub socket: PathBuf,
    /// The D-Bus interface that carries the clipboard
    /// (`org.gnome.Mutter.RemoteDesktop.Session`).
    pub interface: String,
    /// Whether the clipboard has its own `RemoteDesktop` session, apart from
    /// the one that records the outputs (so output changes never touch it).
    pub dedicated_session: bool,
}

const fn startup_monitors() -> VirtualOutputMode {
    VirtualOutputMode::StartupMonitors
}

impl WaylandAgentReady {
    /// Whether every path in the message is inside `runtime_dir`, and the
    /// message names one stream per output with matching geometry.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.wayland_display == WAYLAND_DISPLAY_NAME
            && self.input_socket == self.runtime_dir.join(INPUT_SOCKET_NAME)
            && self.clipboard.as_ref().is_none_or(|clipboard| {
                clipboard.socket == self.runtime_dir.join(CLIPBOARD_SOCKET_NAME)
            })
            && self.output_socket.as_ref().is_none_or(|socket| {
                *socket == self.runtime_dir.join(OUTPUT_SOCKET_NAME)
                    && self.output_mode.reconfigurable()
            })
            && self.outputs.len() <= self.output_mode.max_outputs()
            && !self.outputs.is_empty()
            && self.outputs.len() == self.streams.len()
            && self.generation >= INITIAL_OUTPUT_GENERATION
            && self
                .x11
                .as_ref()
                .is_none_or(|x11| x11.is_consistent(&self.runtime_dir))
            && self
                .outputs
                .iter()
                .zip(&self.streams)
                .all(|(output, stream)| {
                    output.connector == stream.connector
                        && output.width == stream.width
                        && output.height == stream.height
                        && stream.node_id != 0
                })
    }

    /// This session after an output reconfiguration the agent reported:
    /// the same session with `reply`'s outputs, streams and generation.
    #[must_use]
    pub fn with_outputs(&self, reply: &OutputReply) -> Self {
        Self {
            outputs: reply.outputs.clone(),
            streams: reply.streams.clone(),
            generation: reply.generation,
            ..self.clone()
        }
    }

    /// The capture hand-off of the primary (first) stream.
    ///
    /// # Errors
    ///
    /// When the message has no stream or a stream that fails the shared
    /// hand-off validation.
    ///
    /// The compositor's GPU is named only when the agent proved it (exactly
    /// one render node held open by the compositor). The encoder is pinned to
    /// that same GPU only when it is an NVIDIA device
    /// ([`session_encoder_gpu`]); the session environment drops that pin
    /// again when the operator chose a CUDA device.
    pub fn primary_handoff(&self) -> Result<PipeWireCaptureHandoff, CaptureHandoffError> {
        let stream = self
            .streams
            .first()
            .ok_or(CaptureHandoffError::Missing(PIPEWIRE_NODE_ENV))?;
        self.stream_handoff(stream, self.generation)
    }

    /// The capture hand-off of `stream` in output generation `generation`,
    /// with this session's runtime directory and GPU facts: what a capture
    /// epoch of one output after a reconfiguration is given.
    ///
    /// # Errors
    ///
    /// As [`Self::primary_handoff`].
    pub fn stream_handoff(
        &self,
        stream: &ReadyStream,
        generation: u64,
    ) -> Result<PipeWireCaptureHandoff, CaptureHandoffError> {
        let compositor_gpu = self
            .compositor_gpu_pci
            .as_deref()
            .map(|pci| {
                parse_pci_address(pci).ok_or(CaptureHandoffError::Invalid(
                    arcen_media::video::capture_handoff::COMPOSITOR_GPU_PCI_ENV,
                ))
            })
            .transpose()?;
        PipeWireCaptureHandoff::new(
            stream.node_id,
            &stream.connector,
            generation,
            (stream.width, stream.height),
            (stream.x, stream.y),
            self.cursor_mode.screencast(),
            &self.runtime_dir,
        )?
        .with_encoder_gpu(session_encoder_gpu(
            compositor_gpu,
            self.compositor_gpu_vendor,
        ))
        .with_compositor_gpu(compositor_gpu, self.compositor_render_node.as_deref())
    }
}

/// One output of an [`OutputCommand::Configure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputPlacement {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Refresh in hertz.
    pub refresh_hz: u32,
    /// Left edge in desktop pixels.
    pub x: i32,
    /// Top edge in desktop pixels.
    pub y: i32,
    /// The client display's physical width in millimetres (0 unknown).
    #[serde(default)]
    pub width_mm: u32,
    /// The client display's physical height in millimetres (0 unknown).
    #[serde(default)]
    pub height_mm: u32,
    /// The client display's logical width (0 unknown).
    #[serde(default)]
    pub logical_width: u32,
}

impl OutputPlacement {
    /// The checked placement.
    #[must_use]
    pub fn placed(&self) -> Option<PlacedOutput> {
        Some(PlacedOutput {
            request: VirtualOutputRequest::new(self.width, self.height, self.refresh_hz).ok()?,
            x: self.x,
            y: self.y,
            density: arcen_session::wayland_outputs::OutputDensity {
                width_mm: self.width_mm,
                height_mm: self.height_mm,
                logical_width: self.logical_width,
            },
        })
    }

    /// The wire form of `placed`.
    #[must_use]
    pub const fn from_placed(placed: &PlacedOutput) -> Self {
        Self {
            width: placed.request.width(),
            height: placed.request.height(),
            refresh_hz: placed.request.refresh_hz(),
            x: placed.x,
            y: placed.y,
            width_mm: placed.density.width_mm,
            height_mm: placed.density.height_mm,
            logical_width: placed.density.logical_width,
        }
    }
}

/// One request on [`OUTPUT_SOCKET_NAME`], newline-delimited JSON. Only the
/// Pier (root) and the session user may connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputCommand {
    /// Reports the current outputs and generation; changes nothing.
    Hello,
    /// Moves the session to exactly these outputs, as output generation
    /// `generation` (the next one). Applied as one output transaction:
    /// either every output is created, resized and placed and the
    /// generation advances, or the previous outputs are restored and it
    /// does not. No capture epoch may consume the session's streams while
    /// it runs.
    Configure {
        /// The generation the new outputs belong to.
        generation: u64,
        /// The outputs, in order; the first is primary.
        outputs: Vec<OutputPlacement>,
    },
    /// Records every output of generation `generation` again with
    /// `RecordMonitor` for a Grading or HDR capture epoch and answers with
    /// those streams (one per output, the outputs' own connectors, sizes and
    /// places). Refused unless the session's compositor composes in a wide
    /// colour state. The streams live until the next reconfiguration.
    WideStreams {
        /// The output generation the streams must record.
        generation: u64,
    },
}

/// The answer to an [`OutputCommand`]: the outputs that are now live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputReply {
    /// Whether the command was applied.
    pub ok: bool,
    /// Why not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Current output generation.
    pub generation: u64,
    /// Current outputs.
    pub outputs: Vec<ReadyOutput>,
    /// Current streams (one per output; a recreated set has new nodes).
    pub streams: Vec<ReadyStream>,
}

impl OutputReply {
    /// Whether the reply describes a consistent set: one stream per output
    /// with matching connector and size, at a valid generation.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.generation >= INITIAL_OUTPUT_GENERATION
            && !self.outputs.is_empty()
            && self.outputs.len() == self.streams.len()
            && self
                .outputs
                .iter()
                .zip(&self.streams)
                .all(|(output, stream)| {
                    output.connector == stream.connector
                        && output.width == stream.width
                        && output.height == stream.height
                        && stream.node_id != 0
                })
    }
}

/// One session-bound input command, newline-delimited JSON on
/// [`INPUT_SOCKET_NAME`].
///
/// Coordinates are output-local pixels of the named connector's stream and
/// carry the output generation they were mapped for; the session refuses a
/// coordinate of any other generation. Keycodes and buttons are Linux evdev
/// codes; keysyms are X keysyms. A pen sample is injected only when the
/// session proved an emulated stylus ([`StylusFacts`]); Mutter
/// `RemoteDesktop` itself has no tablet device type. Committed text is planned by
/// the session against its compositor's keymap ([`text`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputCommand {
    /// Asks for the session's input facts; answered with
    /// [`InputReply::session`]. Injects nothing.
    Hello,
    /// Absolute pointer motion.
    PointerAbsolute {
        /// Output connector.
        output: String,
        /// Output generation the coordinate was mapped for.
        generation: u64,
        /// X in stream pixels.
        x: f64,
        /// Y in stream pixels.
        y: f64,
    },
    /// Relative pointer motion, in compositor logical pixels.
    PointerRelative {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Pointer button.
    PointerButton {
        /// evdev button code (`BTN_LEFT` = 272).
        button: i32,
        /// Pressed or released.
        pressed: bool,
    },
    /// Smooth scroll, in compositor scroll units (10 per wheel detent).
    PointerAxis {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta, positive downwards.
        dy: f64,
        /// End of a scroll sequence.
        #[serde(default)]
        finish: bool,
        /// Which kind of device the scroll stands for.
        #[serde(default)]
        source: AxisSource,
    },
    /// Discrete scroll steps.
    PointerAxisDiscrete {
        /// 0 vertical, 1 horizontal.
        axis: u32,
        /// Steps, positive downwards or rightwards.
        steps: i32,
    },
    /// Keyboard evdev keycode.
    Key {
        /// evdev keycode (`KEY_A` = 30).
        keycode: u32,
        /// Pressed or released.
        pressed: bool,
    },
    /// Keyboard keysym.
    Keysym {
        /// X keysym.
        keysym: u32,
        /// Pressed or released.
        pressed: bool,
    },
    /// Touch down.
    TouchDown {
        /// Output connector.
        output: String,
        /// Output generation the coordinate was mapped for.
        generation: u64,
        /// Touch slot.
        slot: u32,
        /// X in stream pixels.
        x: f64,
        /// Y in stream pixels.
        y: f64,
    },
    /// Touch motion.
    TouchMotion {
        /// Output connector.
        output: String,
        /// Output generation the coordinate was mapped for.
        generation: u64,
        /// Touch slot.
        slot: u32,
        /// X in stream pixels.
        x: f64,
        /// Y in stream pixels.
        y: f64,
    },
    /// Touch up.
    TouchUp {
        /// Touch slot.
        slot: u32,
    },
    /// One pen sample for the session's emulated stylus, with the Deck's
    /// vendor-neutral axes (`arcen_input::pen_axes`).
    Pen {
        /// Output connector.
        output: String,
        /// Output generation the coordinate was mapped for.
        generation: u64,
        /// X in stream pixels.
        x: f64,
        /// Y in stream pixels.
        y: f64,
        /// The eraser end rather than the tip.
        #[serde(default)]
        eraser: bool,
        /// Within digitizer proximity.
        in_proximity: bool,
        /// Tip touching the surface.
        #[serde(default)]
        touching: bool,
        /// Barrel buttons, as a bitmask.
        #[serde(default)]
        buttons: u16,
        /// Pressure, `0..=1`.
        #[serde(default)]
        pressure: f32,
        /// X tilt in degrees, `-90..=90`.
        #[serde(default)]
        tilt_x: f32,
        /// Y tilt in degrees, `-90..=90`.
        #[serde(default)]
        tilt_y: f32,
        /// Barrel rotation in degrees, `0..=360`.
        #[serde(default)]
        rotation: f32,
    },
    /// Take the pen out of proximity, releasing whatever it holds.
    PenAway,
    /// Release every held key, button and touch.
    ReleaseAll,
    /// Type committed text: the session types what its layout proves
    /// typeable, commits the rest through the input method when it has one,
    /// and answers with [`InputReply::text`]. Never logged.
    Text {
        /// The text (a validated commit).
        text: String,
        /// The client's commit sequence, for its result.
        #[serde(default)]
        sequence: u64,
    },
    /// Asks for the session's keyboard layout; answered with
    /// [`InputReply::keymap`]. Injects nothing.
    Keymap,
    /// Make the session's Caps, Num and Scroll Lock states equal these;
    /// `None` leaves a lock as it is. The session toggles only what
    /// differs from the lock state it tracks.
    Locks {
        /// Caps Lock on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caps_lock: Option<bool>,
        /// Num Lock on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        num_lock: Option<bool>,
        /// Scroll Lock on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scroll_lock: Option<bool>,
    },
}

/// The reply a superseded connection receives, unasked, before the session
/// closes it.
pub const SUPERSEDED_REASON: &str = "superseded by a newer input connection";

/// The reply to a command read after its deadline; it was not injected.
pub const EXPIRED_REASON: &str = "expired before it was read";

/// One line on [`INPUT_SOCKET_NAME`]: a command and the moment after which
/// it must not be injected.
///
/// A command written just before the session stalls stays unread in the
/// socket; when the session resumes, the sender may long have given up on
/// it and reconnected. The deadline is the sender's `CLOCK_MONOTONIC` at
/// sending plus how long it waits for the reply, so a command its sender
/// abandoned is refused instead of injected late. Sender and session run on
/// the same host, so they share the clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRequest {
    /// `CLOCK_MONOTONIC` nanoseconds after which the command is refused.
    pub not_after_ns: u64,
    /// The command.
    pub command: InputCommand,
}

impl InputRequest {
    /// `command`, valid for `validity` from now.
    #[must_use]
    pub fn new(command: InputCommand, validity: Duration) -> Self {
        let validity = u64::try_from(validity.as_nanos()).unwrap_or(u64::MAX);
        Self {
            not_after_ns: monotonic_now_ns().saturating_add(validity),
            command,
        }
    }

    /// Whether the deadline passed at `now_ns`.
    #[must_use]
    pub const fn expired_at(&self, now_ns: u64) -> bool {
        now_ns > self.not_after_ns
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds: one clock for every process on the
/// host.
#[must_use]
pub fn monotonic_now_ns() -> u64 {
    #[cfg(target_os = "linux")]
    {
        nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |now| {
            u64::try_from(now.tv_sec())
                .unwrap_or(0)
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::try_from(now.tv_nsec()).unwrap_or(0))
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No session agent runs off Linux; one process-wide clock serves
        // the tests there.
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        u64::try_from(
            START
                .get_or_init(std::time::Instant::now)
                .elapsed()
                .as_nanos(),
        )
        .unwrap_or(u64::MAX)
    }
}

/// evdev codes of Caps Lock, Num Lock and Scroll Lock.
pub const LOCK_KEYCODES: [u32; 3] = [58, 69, 70];

/// evdev codes of the modifier keys: Shift, Control, Alt and Super, left
/// and right.
pub const MODIFIER_KEYCODES: [u32; 8] = [42, 54, 29, 97, 56, 100, 125, 126];

/// The session's lock states, as the keys delivered to its compositor set
/// them. A headless session has no other keyboard, so after the state it
/// starts with ([`LockState::at_session_start`]) these are the compositor's
/// own states.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LockState([bool; 3]);

/// GNOME's remembered Num Lock (`org.gnome.desktop.peripherals.keyboard`),
/// as read from the session; `None` where a key could not be read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RememberedNumLock {
    /// `remember-numlock-state` (schema default `true`).
    pub remember: Option<bool>,
    /// `numlock-state` (schema default `false`).
    pub state: Option<bool>,
}

impl RememberedNumLock {
    /// Whether the session starts with Num Lock on. Mutter (49:
    /// `meta-seat-impl.c`) turns it on when its seat starts if GNOME
    /// remembers Num Lock and remembered it on, and while it remembers it
    /// writes every change back to `numlock-state`. Caps and Scroll Lock
    /// always start off.
    #[must_use]
    pub fn num_lock_on(self) -> bool {
        self.remember != Some(false) && self.state == Some(true)
    }
}

impl LockState {
    /// The locks a session's compositor starts with.
    #[must_use]
    pub fn at_session_start(remembered: RememberedNumLock) -> Self {
        Self([false, remembered.num_lock_on(), false])
    }

    /// Records a command the compositor accepted: a lock key press toggles
    /// its lock.
    pub fn apply_delivered(&mut self, command: &InputCommand) {
        if let InputCommand::Key {
            keycode,
            pressed: true,
        } = command
        {
            if let Some(index) = LOCK_KEYCODES.iter().position(|code| code == keycode) {
                self.0[index] = !self.0[index];
            }
        }
    }

    /// The key presses and releases that bring the locks to `requested`.
    #[must_use]
    pub fn plan(&self, requested: [Option<bool>; 3]) -> Vec<InputCommand> {
        let mut commands = Vec::new();
        for (index, wanted) in requested.into_iter().enumerate() {
            if wanted.is_some_and(|wanted| wanted != self.0[index]) {
                for pressed in [true, false] {
                    commands.push(InputCommand::Key {
                        keycode: LOCK_KEYCODES[index],
                        pressed,
                    });
                }
            }
        }
        commands
    }

    /// Caps, Num and Scroll Lock.
    #[must_use]
    pub const fn states(&self) -> [bool; 3] {
        self.0
    }
}

/// The device a smooth scroll stands for (Mutter axis source flags).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AxisSource {
    /// A touchpad finger: a sequence ends with `finish`.
    #[default]
    Finger,
    /// A continuous device without sequence ends (precise scroll deltas).
    Continuous,
}

impl AxisSource {
    /// The `RemoteDesktop` `NotifyPointerAxis` flags for this source.
    #[must_use]
    pub const fn mutter_flags(self, finish: bool) -> u32 {
        // Mutter axis flags: bit 0 finish, bit 2 finger, bit 3 continuous.
        let source = match self {
            Self::Finger => 1 << 2,
            Self::Continuous => 1 << 3,
        };
        if finish {
            source | 1
        } else {
            source
        }
    }
}

/// Why an input command was refused before reaching the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRefusal {
    /// The named output is not one of this session's outputs.
    UnknownOutput,
    /// The coordinate was mapped for another output generation.
    StaleGeneration,
    /// A coordinate is not finite or outside the stream.
    OutOfBounds,
    /// A scroll delta or step is not finite or implausibly large.
    InvalidScroll,
    /// A relative motion delta is not finite or implausibly large.
    InvalidMotion,
    /// A pen axis is not finite or outside its range.
    InvalidPen,
    /// Committed text that is empty, too long or not printable.
    InvalidText,
}

impl std::fmt::Display for InputRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UnknownOutput => "unknown output",
            Self::StaleGeneration => "stale output generation",
            Self::OutOfBounds => "coordinate outside the output",
            Self::InvalidScroll => "invalid scroll",
            Self::InvalidMotion => "invalid relative motion",
            Self::InvalidPen => "invalid pen axis",
            Self::InvalidText => "invalid committed text",
        })
    }
}

const MAX_SCROLL_DELTA: f64 = 10_000.0;
const MAX_SCROLL_STEPS: u32 = 1_000;
const MAX_RELATIVE_DELTA: f64 = 100_000.0;

/// Checks a command against the session's streams and output generation.
///
/// # Errors
///
/// [`InputRefusal`] for an unknown output, a stale generation, an
/// out-of-bounds coordinate, or invalid scroll or motion.
pub fn validate_input(
    command: &InputCommand,
    streams: &[ReadyStream],
    generation: u64,
) -> Result<(), InputRefusal> {
    let point = |output: &str, stamped: u64, x: f64, y: f64| {
        let stream = streams
            .iter()
            .find(|stream| stream.connector == output)
            .ok_or(InputRefusal::UnknownOutput)?;
        if stamped != generation {
            return Err(InputRefusal::StaleGeneration);
        }
        if x.is_finite()
            && y.is_finite()
            && (0.0..=f64::from(stream.width)).contains(&x)
            && (0.0..=f64::from(stream.height)).contains(&y)
        {
            Ok(())
        } else {
            Err(InputRefusal::OutOfBounds)
        }
    };
    match command {
        InputCommand::PointerAbsolute {
            output,
            generation,
            x,
            y,
        }
        | InputCommand::TouchDown {
            output,
            generation,
            x,
            y,
            ..
        }
        | InputCommand::TouchMotion {
            output,
            generation,
            x,
            y,
            ..
        } => point(output, *generation, *x, *y),
        InputCommand::Pen {
            output,
            generation,
            x,
            y,
            pressure,
            tilt_x,
            tilt_y,
            rotation,
            ..
        } => {
            point(output, *generation, *x, *y)?;
            let within = |value: f32, low: f32, high: f32| {
                value.is_finite() && (low..=high).contains(&value)
            };
            if within(*pressure, 0.0, 1.0)
                && within(*tilt_x, -90.0, 90.0)
                && within(*tilt_y, -90.0, 90.0)
                && within(*rotation, 0.0, 360.0)
            {
                Ok(())
            } else {
                Err(InputRefusal::InvalidPen)
            }
        }
        InputCommand::PointerRelative { dx, dy } => {
            if dx.is_finite()
                && dy.is_finite()
                && dx.abs() <= MAX_RELATIVE_DELTA
                && dy.abs() <= MAX_RELATIVE_DELTA
            {
                Ok(())
            } else {
                Err(InputRefusal::InvalidMotion)
            }
        }
        InputCommand::PointerAxis { dx, dy, .. } => {
            if dx.is_finite()
                && dy.is_finite()
                && dx.abs() <= MAX_SCROLL_DELTA
                && dy.abs() <= MAX_SCROLL_DELTA
            {
                Ok(())
            } else {
                Err(InputRefusal::InvalidScroll)
            }
        }
        InputCommand::PointerAxisDiscrete { axis, steps } => {
            if *axis <= 1 && steps.unsigned_abs() <= MAX_SCROLL_STEPS {
                Ok(())
            } else {
                Err(InputRefusal::InvalidScroll)
            }
        }
        InputCommand::Hello
        | InputCommand::PointerButton { .. }
        | InputCommand::Key { .. }
        | InputCommand::Keysym { .. }
        | InputCommand::TouchUp { .. }
        | InputCommand::PenAway
        | InputCommand::ReleaseAll
        | InputCommand::Keymap
        | InputCommand::Locks { .. } => Ok(()),
        InputCommand::Text { text, .. } => arcen_protocol::messages::validate_commit_text(text)
            .map_err(|_| InputRefusal::InvalidText),
    }
}

/// Keys, buttons and touches currently held through the input socket.
///
/// A connection that drops or is superseded, or a session that ends,
/// releases everything still held, so the compositor never keeps a stuck
/// key or button.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeldInput {
    keycodes: BTreeSet<u32>,
    keysyms: BTreeSet<u32>,
    buttons: BTreeSet<i32>,
    touches: BTreeSet<u32>,
    pen: bool,
}

impl HeldInput {
    /// Records a press before it is sent to the compositor, so a press whose
    /// delivery is interrupted is still released later. Releasing a key the
    /// compositor never saw pressed is harmless; never releasing one it did
    /// is not.
    pub fn apply_before(&mut self, command: &InputCommand) {
        match command {
            InputCommand::Key {
                keycode,
                pressed: true,
            } => {
                self.keycodes.insert(*keycode);
            }
            InputCommand::Keysym {
                keysym,
                pressed: true,
            } => {
                self.keysyms.insert(*keysym);
            }
            InputCommand::PointerButton {
                button,
                pressed: true,
            } => {
                self.buttons.insert(*button);
            }
            InputCommand::TouchDown { slot, .. } => {
                self.touches.insert(*slot);
            }
            InputCommand::Pen {
                in_proximity: true, ..
            } => self.pen = true,
            _ => {}
        }
    }

    /// Records a command the compositor accepted.
    pub fn apply(&mut self, command: &InputCommand) {
        match command {
            InputCommand::Key { keycode, pressed } => {
                toggle(&mut self.keycodes, *keycode, *pressed);
            }
            InputCommand::Keysym { keysym, pressed } => {
                toggle(&mut self.keysyms, *keysym, *pressed);
            }
            InputCommand::PointerButton { button, pressed } => {
                toggle(&mut self.buttons, *button, *pressed);
            }
            InputCommand::TouchDown { slot, .. } => {
                self.touches.insert(*slot);
            }
            InputCommand::TouchUp { slot } => {
                self.touches.remove(slot);
            }
            InputCommand::Pen { in_proximity, .. } => self.pen = *in_proximity,
            InputCommand::PenAway => self.pen = false,
            InputCommand::ReleaseAll => *self = Self::default(),
            InputCommand::Locks { .. }
            | InputCommand::Text { .. }
            | InputCommand::Keymap
            | InputCommand::Hello
            | InputCommand::PointerAbsolute { .. }
            | InputCommand::PointerRelative { .. }
            | InputCommand::PointerAxis { .. }
            | InputCommand::PointerAxisDiscrete { .. }
            | InputCommand::TouchMotion { .. } => {}
        }
    }

    /// The modifier keys held, in [`MODIFIER_KEYCODES`] order.
    #[must_use]
    pub fn held_modifiers(&self) -> Vec<u32> {
        MODIFIER_KEYCODES
            .into_iter()
            .filter(|code| self.keycodes.contains(code))
            .collect()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keycodes.is_empty()
            && self.keysyms.is_empty()
            && self.buttons.is_empty()
            && self.touches.is_empty()
            && !self.pen
    }

    /// The commands that release everything held, and forgets it.
    pub fn take_releases(&mut self) -> Vec<InputCommand> {
        let held = std::mem::take(self);
        held.keycodes
            .into_iter()
            .map(|keycode| InputCommand::Key {
                keycode,
                pressed: false,
            })
            .chain(held.keysyms.into_iter().map(|keysym| InputCommand::Keysym {
                keysym,
                pressed: false,
            }))
            .chain(
                held.buttons
                    .into_iter()
                    .map(|button| InputCommand::PointerButton {
                        button,
                        pressed: false,
                    }),
            )
            .chain(
                held.touches
                    .into_iter()
                    .map(|slot| InputCommand::TouchUp { slot }),
            )
            .chain(held.pen.then_some(InputCommand::PenAway))
            .collect()
    }
}

fn toggle<T: Ord>(set: &mut BTreeSet<T>, value: T, pressed: bool) {
    if pressed {
        set.insert(value);
    } else {
        set.remove(&value);
    }
}

/// Which compositor interface the session injects through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputBackendKind {
    /// Mutter `RemoteDesktop` `Notify*` D-Bus calls.
    RemoteDesktopDbus,
    /// An EIS (libei) connection from `RemoteDesktop.Session.ConnectToEIS`.
    /// Not implemented; named so a session can never be mistaken for one.
    Eis,
}

/// What the session established about an emulated stylus, from its
/// compositor's EIS seat (`ConnectToEIS`, libei `ei_stylus`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StylusFacts {
    /// The capabilities bound on the compositor's stylus; `None` when the
    /// session cannot inject pen samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<arcen_input::pen_emulation::StylusBinding>,
    /// Why not, when `binding` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<arcen_input::pen_emulation::StylusUnavailable>,
    /// The `ei_stylus` interface version the compositor negotiated, when it
    /// offered one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ei_stylus_version: Option<u32>,
    /// The interfaces the compositor's EIS seat offered, as evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seat_interfaces: Vec<String>,
}

impl StylusFacts {
    /// A session without a stylus, and why.
    #[must_use]
    pub const fn unavailable(why: arcen_input::pen_emulation::StylusUnavailable) -> Self {
        Self {
            binding: None,
            unavailable: Some(why),
            ei_stylus_version: None,
            seat_interfaces: Vec::new(),
        }
    }
}

/// What the session's input service proves to a connecting Pier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSessionFacts {
    /// Output generation every coordinate must carry.
    pub generation: u64,
    /// The session's streams, in connector order.
    pub streams: Vec<ReadyStream>,
    /// `RemoteDesktop` `SupportedDeviceTypes`.
    pub device_types: u32,
    /// The `RemoteDesktop` session was started and linked to the
    /// `ScreenCast` session, and accepted an absolute motion to a stream.
    pub remote_desktop_started: bool,
    /// The injection backend.
    pub backend: InputBackendKind,
    /// Whether the compositor offers `ConnectToEIS`; `None` when it could
    /// not be established.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eis_offered: Option<bool>,
    /// The session's emulated stylus; `None` from an agent that predates
    /// pen support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stylus: Option<StylusFacts>,
    /// This connection's number; a newer connection supersedes it.
    pub connection: u64,
    /// Committed text: present when the session types it; an agent that
    /// predates it leaves it out, and the Pier then refuses committed text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<text::TextFacts>,
}

/// Reply line for one input command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputReply {
    /// Accepted by the compositor.
    pub ok: bool,
    /// Why not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The session's input facts, in reply to [`InputCommand::Hello`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<InputSessionFacts>,
    /// What became of a commit, in reply to [`InputCommand::Text`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<text::TextOutcome>,
    /// The session's layout, in reply to [`InputCommand::Keymap`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keymap: Option<text::SessionKeymap>,
    /// How many keymaps the session has read from its compositor, on every
    /// reply of a session that types committed text, so the sender notices
    /// a layout change on its next command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keymap_generation: Option<u64>,
}

impl InputReply {
    /// An accepted command.
    #[must_use]
    pub const fn accepted() -> Self {
        Self {
            ok: true,
            error: None,
            session: None,
            text: None,
            keymap: None,
            keymap_generation: None,
        }
    }

    /// A refused command.
    #[must_use]
    pub fn refused(reason: &str) -> Self {
        Self {
            ok: false,
            error: Some(reason.to_owned()),
            session: None,
            text: None,
            keymap: None,
            keymap_generation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_probe_of_a_side_by_side_prefix_runs_as_nobody() {
        let prefix = CompositorPrefix::parse("/opt/arcen-gnome/51").unwrap();
        assert!(probe_as_nobody(
            &prefix,
            "/opt/arcen-gnome/51/bin/gnome-shell",
            true
        ));
        assert!(!probe_as_nobody(
            &prefix,
            "/opt/arcen-gnome/51/bin/gnome-shell",
            false
        ));
        assert!(!probe_as_nobody(
            &prefix,
            "/opt/arcen-gnome/510/bin/mutter",
            true
        ));
        assert!(!probe_as_nobody(&prefix, "/usr/bin/pipewire", true));
        assert!(!probe_as_nobody(
            &CompositorPrefix::system(),
            "/usr/bin/gnome-shell",
            true
        ));
    }

    #[test]
    fn num_lock_starts_as_gnome_remembered_it() {
        let start = |remember, state| {
            LockState::at_session_start(RememberedNumLock { remember, state }).states()
        };
        assert_eq!(start(Some(true), Some(true)), [false, true, false]);
        // The schema defaults: remembered, and remembered off.
        assert_eq!(start(None, Some(true)), [false, true, false]);
        assert_eq!(start(Some(true), None), [false; 3]);
        assert_eq!(start(Some(false), Some(true)), [false; 3]);
        assert_eq!(start(Some(true), Some(false)), [false; 3]);
    }

    #[test]
    fn the_x11_plan_is_settled_for_every_wayland_selection_and_only_for_it() {
        use arcen_session::desktop_provider::DesktopProvider;
        use arcen_session::wayland_x11::X11AppsDisabledReason;
        for setting in [
            X11AppsSetting::Auto,
            X11AppsSetting::On,
            X11AppsSetting::Off,
        ] {
            assert_eq!(
                configured_x11_plan(DesktopProvider::Xorg, setting, None),
                Ok(None)
            );
        }
        assert_eq!(
            configured_x11_plan(DesktopProvider::Wayland, X11AppsSetting::Off, None),
            Ok(Some(X11AppsPlan::Disabled(
                X11AppsDisabledReason::OperatorOff
            )))
        );
        // No record proves Xwayland, so `auto` is off and says why...
        assert!(matches!(
            configured_x11_plan(DesktopProvider::Wayland, X11AppsSetting::Auto, None),
            Ok(Some(X11AppsPlan::Disabled(
                X11AppsDisabledReason::NotQualified | X11AppsDisabledReason::XwaylandNotInstalled
            )))
        ));
        // ...and `on` refuses to start.
        assert!(configured_x11_plan(DesktopProvider::Wayland, X11AppsSetting::On, None).is_err());
    }

    #[test]
    fn a_wide_streams_request_names_its_generation_on_the_wire() {
        let command = OutputCommand::WideStreams { generation: 3 };
        let line = serde_json::to_string(&command).unwrap();
        assert_eq!(line, r#"{"op":"wide_streams","generation":3}"#);
        assert_eq!(
            serde_json::from_str::<OutputCommand>(&line).unwrap(),
            command
        );
        assert!(serde_json::from_str::<OutputCommand>(r#"{"op":"wide_streams"}"#).is_err());
    }

    fn stream() -> ReadyStream {
        ReadyStream {
            connector: "Meta-0".into(),
            node_id: 44,
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
        }
    }

    #[test]
    fn stream_cursor_mode_tokens_and_ready_default() {
        assert_eq!(StreamCursorMode::default(), StreamCursorMode::Embedded);
        for mode in [
            StreamCursorMode::Embedded,
            StreamCursorMode::Metadata,
            StreamCursorMode::Hidden,
        ] {
            assert_eq!(StreamCursorMode::from_token(mode.token()), Some(mode));
            assert_eq!(
                StreamCursorMode::from_screencast(mode.screencast()),
                Some(mode)
            );
        }
        assert_eq!(StreamCursorMode::from_token("none"), None);
        // A ready message without the field (an older agent) is embedded.
        let mut json = serde_json::to_value(ready()).unwrap();
        json.as_object_mut().unwrap().remove("cursor_mode");
        let parsed: WaylandAgentReady = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.cursor_mode, StreamCursorMode::Embedded);
        // The capture hand-off follows the session's mode.
        let metadata = WaylandAgentReady {
            cursor_mode: StreamCursorMode::Metadata,
            ..ready()
        };
        assert_eq!(
            metadata.primary_handoff().unwrap().cursor_mode(),
            ScreenCastCursorMode::Metadata
        );
    }

    fn ready() -> WaylandAgentReady {
        let runtime_dir = private_runtime_dir(1000, "c7");
        WaylandAgentReady {
            compositor: "gnome-shell".into(),
            input_socket: runtime_dir.join(INPUT_SOCKET_NAME),
            runtime_dir,
            wayland_display: WAYLAND_DISPLAY_NAME.into(),
            outputs: vec![ReadyOutput {
                connector: "Meta-0".into(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }],
            streams: vec![stream()],
            input_device_types: 7,
            generation: INITIAL_OUTPUT_GENERATION,
            compositor_render_node: None,
            compositor_gpu_pci: None,
            compositor_gpu_vendor: None,
            output_mode: arcen_session::wayland_outputs::VirtualOutputMode::StartupMonitors,
            output_socket: None,
            x11: None,
            cursor_mode: Default::default(),
            clipboard: None,
        }
    }

    #[test]
    fn the_x11_part_of_the_ready_message_is_checked() {
        let runtime_dir = private_runtime_dir(1000, "c7");
        let displays = SessionDisplays {
            public: 0,
            private: 1,
            lock_pid: 4242,
        };
        let served = ReadyX11 {
            ready: true,
            failure: None,
            displays: Some(displays),
            xauthority: Some(runtime_dir.join(".mutter-Xwaylandauth.F8GGX3")),
            glamor: GlamorRoute::NvidiaGbm,
        };
        let mut message = ready();
        message.x11 = Some(served.clone());
        assert!(message.is_consistent());
        assert_eq!(served.display().as_deref(), Some(":0"));
        assert_eq!(served.setup_display().as_deref(), Some(":1"));
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("\"glamor\":\"nvidia-gbm\""));
        assert_eq!(
            serde_json::from_str::<WaylandAgentReady>(&json).unwrap(),
            message
        );
        assert!(!serde_json::to_string(&ready()).unwrap().contains("x11"));

        let failed = ReadyX11::failed("display :0 did not answer", Some(displays));
        message.x11 = Some(failed);
        assert!(message.is_consistent());

        for bad in [
            ReadyX11 {
                xauthority: Some(PathBuf::from("/tmp/.mutter-Xwaylandauth.F8GGX3")),
                ..served.clone()
            },
            ReadyX11 {
                xauthority: None,
                ..served.clone()
            },
            ReadyX11 {
                displays: Some(SessionDisplays {
                    public: 2,
                    private: 1,
                    lock_pid: 4242,
                }),
                ..served.clone()
            },
            ReadyX11 {
                failure: Some("both".into()),
                ..served.clone()
            },
        ] {
            message.x11 = Some(bad.clone());
            assert!(!message.is_consistent(), "{bad:?}");
        }
    }

    #[test]
    fn runtime_dir_is_session_scoped() {
        let dir = private_runtime_dir(1000, "c7");
        assert_eq!(dir, PathBuf::from("/run/user/1000/arcen-wayland-c7"));
        assert!(is_private_runtime_dir(&dir, 1000, "c7"));
        assert!(!is_private_runtime_dir(&dir, 1001, "c7"));
        assert!(!is_private_runtime_dir(
            Path::new("/run/user/1000/arcen-wayland-../x"),
            1000,
            "../x"
        ));
    }

    #[test]
    fn only_the_users_fuse_mounts_inside_the_runtime_dir_are_unmountable() {
        let dir = private_runtime_dir(1000, "7");
        let mountinfo = "\
22 1 253:0 / / rw,relatime shared:1 - xfs /dev/mapper/root rw,attr2
40 22 0:44 / /run/user/1000 rw,nosuid,nodev,relatime shared:2 - tmpfs tmpfs rw,size=1G,uid=1000,gid=1000,mode=700
41 40 0:45 / /run/user/1000/arcen-wayland-7/doc rw,nosuid,nodev,relatime shared:3 - fuse.portal portal rw,user_id=1000,group_id=1000
42 40 0:46 / /run/user/1000/arcen-wayland-7/a\\040b/x rw,relatime shared:4 - fuse sshfs rw,user_id=1000,group_id=1000
43 40 0:47 / /run/user/1000/arcen-wayland-7/other rw,relatime shared:5 - fuse.portal portal rw,user_id=1001,group_id=1001
44 40 8:1 / /run/user/1000/arcen-wayland-7/boot rw,relatime shared:6 - vfat /dev/sda1 rw,fmask=0077
45 40 0:48 / /run/user/1000/arcen-wayland-70/doc rw,relatime shared:7 - fuse.portal portal rw,user_id=1000,group_id=1000
46 40 0:49 / /run/user/1000/arcen-wayland-7/bad\\9x rw,relatime shared:8 - fuse.portal portal rw,user_id=1000,group_id=1000
47 40 0:50 / /run/user/1000/arcen-wayland-7/spoof rw,relatime shared:9 - fuseblk /dev/sdb1 rw,user_id=10000
not a mountinfo line
";
        assert_eq!(
            user_fuse_mounts_inside(mountinfo, &dir, 1000),
            vec![dir.join("a b/x"), dir.join("doc"),],
            "only fuse/fuse.* mounts owned by uid 1000, inside the directory, deepest first"
        );
        assert!(user_fuse_mounts_inside(mountinfo, &dir, 0).is_empty());
    }

    #[test]
    fn a_failed_probe_blocks_only_the_compositor_it_would_have_chosen() {
        use arcen_session::wayland_session::CompositorProbe;
        let probed = |compositor, usable| {
            ProbeOutcome::Probed(CompositorProbe {
                compositor,
                headless: usable,
                virtual_monitor: usable,
            })
        };
        let shell = HeadlessCompositor::GnomeShell;
        let mutter = HeadlessCompositor::Mutter;
        assert_eq!(
            choose_from_outcomes(&[(shell, probed(shell, true)), (mutter, ProbeOutcome::Failed)]),
            Ok(shell)
        );
        assert_eq!(
            choose_from_outcomes(&[
                (shell, ProbeOutcome::Failed),
                (mutter, probed(mutter, true))
            ]),
            Err(CompositorChoiceError::ProbeFailed(shell))
        );
        assert_eq!(
            choose_from_outcomes(&[
                (shell, ProbeOutcome::NotInstalled),
                (mutter, probed(mutter, true))
            ]),
            Ok(mutter)
        );
        assert_eq!(
            choose_from_outcomes(&[
                (shell, probed(shell, false)),
                (mutter, ProbeOutcome::Failed)
            ]),
            Err(CompositorChoiceError::ProbeFailed(mutter))
        );
        assert_eq!(
            choose_from_outcomes(&[
                (shell, ProbeOutcome::NotInstalled),
                (mutter, ProbeOutcome::NotInstalled)
            ]),
            Err(CompositorChoiceError::NoneUsable)
        );
    }

    #[test]
    fn unified_cgroup_is_parsed_strictly() {
        assert_eq!(
            unified_cgroup("0::/user.slice/user-1000.slice/session-7.scope\n"),
            Some(PathBuf::from(
                "/sys/fs/cgroup/user.slice/user-1000.slice/session-7.scope"
            ))
        );
        assert_eq!(unified_cgroup("1:name=systemd:/user.slice\n"), None);
        assert_eq!(unified_cgroup("0::relative/path\n"), None);
        assert_eq!(unified_cgroup("0::/user.slice/../system.slice\n"), None);
    }

    #[test]
    fn wayland_start_budgets_nest() {
        assert!(WAYLAND_START_BUDGET < WAYLAND_AGENT_READY_TIMEOUT);
        assert!(WAYLAND_AGENT_READY_TIMEOUT < WAYLAND_OPEN_TIMEOUT);
    }

    #[test]
    fn outputs_round_trip_and_are_bounded() {
        let output = parse_output("2560x1440@120").unwrap();
        assert_eq!(format_output(&output), "2560x1440@120");
        for bad in [
            "2560x1440",
            "0x1080@60",
            "1920x1080@0",
            "+1920x1080@60",
            "1920x1080@60x",
            "99999x1080@60",
        ] {
            assert!(parse_output(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_native_seat_compositor_is_a_display_server_not_headless() {
        let arguments = native_seat_arguments(compositor_arguments(
            &[],
            VirtualOutputMode::RemoteMonitors,
            true,
        ));
        assert_eq!(
            arguments,
            [
                "--display-server",
                "--wayland",
                "--wayland-display",
                "wayland-0"
            ]
        );
        assert!(!arguments.iter().any(|argument| argument == "--headless"));
    }

    #[test]
    fn compositor_runs_headless_wayland_only_with_named_outputs() {
        let arguments = compositor_arguments(
            &[parse_output("1920x1080@60").unwrap()],
            VirtualOutputMode::StartupMonitors,
            false,
        );
        assert_eq!(
            arguments,
            [
                "--headless",
                "--wayland",
                "--no-x11",
                "--wayland-display",
                "wayland-0",
                "--virtual-monitor",
                "1920x1080@60"
            ]
        );
        let with_x11 = compositor_arguments(
            &[parse_output("1920x1080@60").unwrap()],
            VirtualOutputMode::StartupMonitors,
            true,
        );
        assert_eq!(
            with_x11,
            [
                "--headless",
                "--wayland",
                "--wayland-display",
                "wayland-0",
                "--virtual-monitor",
                "1920x1080@60"
            ]
        );
    }

    #[test]
    fn a_remote_monitor_compositor_starts_without_startup_monitors() {
        let arguments = compositor_arguments(
            &[parse_output("1920x1080@60").unwrap()],
            VirtualOutputMode::RemoteMonitors,
            false,
        );
        assert!(!arguments
            .iter()
            .any(|argument| argument == "--virtual-monitor"));
        assert!(arguments.iter().any(|argument| argument == "--headless"));
    }

    #[test]
    fn placed_outputs_round_trip_and_default_to_the_origin() {
        let placed = parse_placed_output("1280x800@120+1920+140").unwrap();
        assert_eq!(
            (placed.request.width(), placed.request.refresh_hz()),
            (1280, 120)
        );
        assert_eq!((placed.x, placed.y), (1920, 140));
        assert_eq!(format_placed_output(&placed), "1280x800@120+1920+140");
        let origin = parse_placed_output("1920x1080@60").unwrap();
        assert_eq!((origin.x, origin.y), (0, 0));
        assert!(!origin.density.is_known());
        let dense = parse_placed_output("3024x1964@120+0+0~302x196~1512").unwrap();
        assert_eq!(
            dense.density,
            arcen_session::wayland_outputs::OutputDensity {
                width_mm: 302,
                height_mm: 196,
                logical_width: 1512
            }
        );
        assert_eq!(
            format_placed_output(&dense),
            "3024x1964@120+0+0~302x196~1512"
        );
        assert_eq!(
            OutputPlacement::from_placed(&dense).placed(),
            Some(dense),
            "the output socket carries the density too"
        );
        for bad in [
            "1920x1080@60+1",
            "1920x1080@60+-1+0",
            "1920x1080@60+1+2+3",
            "1920x1080@60++",
            "1920x1080@60+0+0~302x196",
            "1920x1080@60+0+0~302~1512",
        ] {
            assert!(parse_placed_output(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn help_probe_needs_option_lines_not_mentions() {
        let help = "  --headless                Run as a headless display server\n  \
                    --virtual-monitor         Add persistent virtual monitor\n";
        assert_eq!(parse_compositor_help(help), (true, true));
        assert_eq!(
            parse_compositor_help("  --nested  no --headless here\n"),
            (false, false)
        );
    }

    #[test]
    fn ready_message_is_checked_for_consistency() {
        assert!(ready().is_consistent());
        let mut wrong_socket = ready();
        wrong_socket.input_socket = PathBuf::from("/run/input.sock");
        assert!(!wrong_socket.is_consistent());
        let mut wrong_size = ready();
        wrong_size.streams[0].width = 1280;
        assert!(!wrong_size.is_consistent());
        let mut no_stream = ready();
        no_stream.streams.clear();
        assert!(!no_stream.is_consistent());
        let handoff = ready().primary_handoff().unwrap();
        assert_eq!(handoff.node_id(), 44);
        assert_eq!(handoff.output().as_str(), "Meta-0");
        assert_eq!(handoff.cursor_mode(), ScreenCastCursorMode::Embedded);
        assert_eq!(handoff.runtime_dir(), private_runtime_dir(1000, "c7"));
        assert_eq!(handoff.encoder_gpu_pci(), None);
        assert_eq!(handoff.compositor_gpu_pci(), None);

        let mut proven = ready();
        proven.compositor_render_node = Some(PathBuf::from("/dev/dri/renderD128"));
        proven.compositor_gpu_pci = Some("0000:00:10.0".into());
        proven.compositor_gpu_vendor = Some(NVIDIA_PCI_VENDOR);
        let handoff = proven.primary_handoff().unwrap();
        assert_eq!(handoff.compositor_gpu_pci(), handoff.encoder_gpu_pci());
        assert!(handoff.compositor_gpu_pci().is_some());
        assert_eq!(
            handoff.compositor_render_node(),
            Some(Path::new("/dev/dri/renderD128"))
        );
        proven.compositor_gpu_pci = Some("not-a-pci".into());
        assert!(proven.primary_handoff().is_err());
    }

    #[test]
    fn a_non_nvidia_compositor_gpu_names_no_encoder() {
        // An emulated VGA (1234:1111), an Intel iGPU, or a vendor sysfs did
        // not state: the compositor facts are still handed over so capenc
        // can take its cross-GPU route, but the encoder stays capenc's own
        // choice.
        for vendor in [Some(0x1234), Some(0x8086), None] {
            let mut ready = ready();
            ready.compositor_render_node = Some(PathBuf::from("/dev/dri/renderD129"));
            ready.compositor_gpu_pci = Some("0000:00:02.0".into());
            ready.compositor_gpu_vendor = vendor;
            let handoff = ready.primary_handoff().unwrap();
            assert_eq!(handoff.encoder_gpu_pci(), None, "{vendor:?}");
            assert_eq!(
                handoff.compositor_gpu_pci(),
                parse_pci_address("0000:00:02.0")
            );
            assert_eq!(
                handoff.compositor_render_node(),
                Some(Path::new("/dev/dri/renderD129"))
            );
            let env = handoff.to_env();
            assert!(!env
                .iter()
                .any(|(key, _)| *key == arcen_capenc::CAPTURE_GPU_PCI_ENV));
        }
        let nvidia = parse_pci_address("0000:41:00.0");
        assert_eq!(session_encoder_gpu(nvidia, Some(NVIDIA_PCI_VENDOR)), nvidia);
        assert_eq!(session_encoder_gpu(None, Some(NVIDIA_PCI_VENDOR)), None);
    }

    #[test]
    fn render_node_gpu_reads_pci_and_vendor_from_sysfs() {
        let sys = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-wayland-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sys);
        let device = sys.join("devices/pci0000:00/0000:00:02.0");
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("vendor"), "0x1234\n").unwrap();
        let class = sys.join("class/drm/renderD129");
        std::fs::create_dir_all(&class).unwrap();
        std::os::unix::fs::symlink(&device, class.join("device")).unwrap();

        assert_eq!(
            render_node_gpu(&sys, Path::new("/dev/dri/renderD129")),
            (Some("0000:00:02.0".to_owned()), Some(0x1234))
        );
        assert_eq!(
            render_node_gpu(&sys, Path::new("/dev/dri/renderD128")),
            (None, None)
        );
        std::fs::remove_dir_all(&sys).unwrap();
    }

    #[test]
    fn only_one_held_render_node_names_the_compositor_gpu() {
        fn paths(items: &[&'static str]) -> Vec<&'static Path> {
            items.iter().copied().map(Path::new).collect()
        }
        let one = paths(&[
            "/dev/null",
            "/dev/dri/renderD128",
            "/dev/dri/renderD128",
            "socket:[1]",
        ]);
        assert_eq!(
            single_render_node(one.iter().copied()),
            Some(PathBuf::from("/dev/dri/renderD128"))
        );
        let none = paths(&["/dev/dri/card0", "/dev/nvidia0"]);
        assert_eq!(single_render_node(none.iter().copied()), None);
        let two = paths(&["/dev/dri/renderD128", "/dev/dri/renderD129"]);
        assert_eq!(single_render_node(two.iter().copied()), None);
    }

    #[test]
    fn input_commands_parse_strictly() {
        let command: InputCommand =
            serde_json::from_str(r#"{"op":"key","keycode":30,"pressed":true}"#).unwrap();
        assert_eq!(
            command,
            InputCommand::Key {
                keycode: 30,
                pressed: true
            }
        );
        assert!(serde_json::from_str::<InputCommand>(r#"{"op":"key","keycode":30}"#).is_err());
        assert!(serde_json::from_str::<InputCommand>(
            r#"{"op":"key","keycode":30,"pressed":true,"extra":1}"#
        )
        .is_err());
        assert!(serde_json::from_str::<InputCommand>(r#"{"op":"pen"}"#).is_err());
    }

    #[test]
    fn input_is_bounded_to_the_session_outputs_and_generation() {
        let streams = [stream()];
        let at = |output: &str, generation: u64, x: f64, y: f64| InputCommand::PointerAbsolute {
            output: output.into(),
            generation,
            x,
            y,
        };
        assert_eq!(
            validate_input(&at("Meta-0", 1, 960.0, 540.0), &streams, 1),
            Ok(())
        );
        assert_eq!(
            validate_input(&at("Meta-1", 1, 1.0, 1.0), &streams, 1),
            Err(InputRefusal::UnknownOutput)
        );
        assert_eq!(
            validate_input(&at("Meta-0", 1, 1.0, 1.0), &streams, 2),
            Err(InputRefusal::StaleGeneration)
        );
        assert_eq!(
            validate_input(
                &InputCommand::TouchDown {
                    output: "Meta-0".into(),
                    generation: 0,
                    slot: 0,
                    x: 1.0,
                    y: 1.0
                },
                &streams,
                1
            ),
            Err(InputRefusal::StaleGeneration)
        );
        assert_eq!(
            validate_input(&at("Meta-0", 1, 1921.0, 1.0), &streams, 1),
            Err(InputRefusal::OutOfBounds)
        );
        assert_eq!(
            validate_input(&at("Meta-0", 1, f64::NAN, 1.0), &streams, 1),
            Err(InputRefusal::OutOfBounds)
        );
        assert_eq!(
            validate_input(
                &InputCommand::PointerAxis {
                    dx: 0.0,
                    dy: f64::INFINITY,
                    finish: false,
                    source: AxisSource::Finger,
                },
                &streams,
                1
            ),
            Err(InputRefusal::InvalidScroll)
        );
        assert_eq!(
            validate_input(
                &InputCommand::PointerAxisDiscrete { axis: 2, steps: 1 },
                &streams,
                1
            ),
            Err(InputRefusal::InvalidScroll)
        );
        assert_eq!(
            validate_input(
                &InputCommand::PointerRelative {
                    dx: f64::NAN,
                    dy: 0.0
                },
                &streams,
                1
            ),
            Err(InputRefusal::InvalidMotion)
        );
        assert_eq!(validate_input(&InputCommand::Hello, &streams, 1), Ok(()));
    }

    #[test]
    fn axis_sources_map_to_mutter_flags() {
        assert_eq!(AxisSource::Finger.mutter_flags(false), 4);
        assert_eq!(AxisSource::Finger.mutter_flags(true), 5);
        assert_eq!(AxisSource::Continuous.mutter_flags(false), 8);
        let legacy: InputCommand =
            serde_json::from_str(r#"{"op":"pointer_axis","dx":0.0,"dy":10.0,"finish":true}"#)
                .unwrap();
        assert_eq!(
            legacy,
            InputCommand::PointerAxis {
                dx: 0.0,
                dy: 10.0,
                finish: true,
                source: AxisSource::Finger
            }
        );
    }

    #[test]
    fn a_press_is_held_before_delivery_and_a_release_only_after() {
        let mut held = HeldInput::default();
        let press = InputCommand::Key {
            keycode: 30,
            pressed: true,
        };
        held.apply_before(&press);
        assert!(!held.is_empty(), "an interrupted press is still released");
        held.apply_before(&InputCommand::Key {
            keycode: 30,
            pressed: false,
        });
        assert!(!held.is_empty(), "a release counts only once delivered");
        held.apply(&InputCommand::Key {
            keycode: 30,
            pressed: false,
        });
        assert!(held.is_empty());
    }

    fn pen(in_proximity: bool, pressure: f32) -> InputCommand {
        InputCommand::Pen {
            output: "Meta-0".into(),
            generation: 1,
            x: 960.0,
            y: 540.0,
            eraser: false,
            in_proximity,
            touching: in_proximity,
            buttons: 0,
            pressure,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
        }
    }

    #[test]
    fn pen_samples_parse_and_are_bounded_like_pointers() {
        let line =
            r#"{"op":"pen","output":"Meta-0","generation":1,"x":5.0,"y":6.0,"in_proximity":true}"#;
        let parsed: InputCommand = serde_json::from_str(line).unwrap();
        assert!(matches!(
            parsed,
            InputCommand::Pen {
                eraser: false,
                touching: false,
                buttons: 0,
                ..
            }
        ));
        let streams = [stream()];
        assert_eq!(validate_input(&pen(true, 0.5), &streams, 1), Ok(()));
        assert_eq!(
            validate_input(&pen(true, 0.5), &streams, 2),
            Err(InputRefusal::StaleGeneration)
        );
        assert_eq!(
            validate_input(&pen(true, 1.5), &streams, 1),
            Err(InputRefusal::InvalidPen)
        );
        assert_eq!(
            validate_input(&pen(true, f32::NAN), &streams, 1),
            Err(InputRefusal::InvalidPen)
        );
        assert_eq!(validate_input(&InputCommand::PenAway, &streams, 9), Ok(()));
    }

    #[test]
    fn a_pen_in_proximity_is_held_and_taken_away_on_release() {
        let mut held = HeldInput::default();
        held.apply_before(&pen(true, 0.5));
        assert!(!held.is_empty());
        assert_eq!(held.clone().take_releases(), vec![InputCommand::PenAway]);
        held.apply(&pen(false, 0.0));
        assert!(held.is_empty());
        held.apply(&pen(true, 0.5));
        held.apply(&InputCommand::PenAway);
        assert!(held.is_empty());
    }

    #[test]
    fn held_input_is_released_deterministically() {
        let mut held = HeldInput::default();
        for command in [
            InputCommand::Key {
                keycode: 42,
                pressed: true,
            },
            InputCommand::Key {
                keycode: 30,
                pressed: true,
            },
            InputCommand::Key {
                keycode: 30,
                pressed: false,
            },
            InputCommand::PointerButton {
                button: 272,
                pressed: true,
            },
            InputCommand::TouchDown {
                output: "Meta-0".into(),
                generation: 1,
                slot: 3,
                x: 1.0,
                y: 1.0,
            },
        ] {
            held.apply(&command);
        }
        assert!(!held.is_empty());
        assert_eq!(
            held.take_releases(),
            vec![
                InputCommand::Key {
                    keycode: 42,
                    pressed: false
                },
                InputCommand::PointerButton {
                    button: 272,
                    pressed: false
                },
                InputCommand::TouchUp { slot: 3 },
            ]
        );
        assert!(held.is_empty());
        held.apply(&InputCommand::Keysym {
            keysym: 0x61,
            pressed: true,
        });
        held.apply(&InputCommand::ReleaseAll);
        assert!(held.is_empty());
    }
}
