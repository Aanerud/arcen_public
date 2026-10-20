//! The Linux installer's desktop-provider step: host facts for the shared
//! plan (`arcen_session::install_provider`), the embedded Pier's runtime
//! check, the Wayland qualification run, and the install log.
//!
//! Everything here gathers facts or runs the Pier; the decisions and their
//! wording are shared and pure. Nothing here changes driver, kernel,
//! device, SELinux or boot configuration, or installs packages.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arcen_session::compositor_prefix::{
    CompositorPrefix, EntryKind, PathOwnership, PrefixInspector, verify_prefix,
};
use arcen_session::desktop_provider::{ProviderPreference, WELL_KNOWN_XORG_SERVERS};
use arcen_session::install_provider::{
    InstallHostFacts, PreferenceSource, RenderNode, kernel_fb_create_format_info,
};
use arcen_session::wayland_qualification::{
    GpuDriver, parse_compositor_version, parse_pipewire_version,
};
use arcen_session::wayland_session::{
    CompositorProbe, DBUS_DAEMON_BINARY, LOGIND_SEATS_DIR, PIPEWIRE_BINARY, VirtualOutputRequest,
    WAYLAND_HOST_COMPOSITORS, WIREPLUMBER_BINARY, WaylandHostProbe, parse_compositor_help,
};

/// Appended by every install, upgrade and uninstall on the real root.
pub const INSTALL_LOG: &str = "/var/log/arcen/install.log";
/// The packaged `platform.session.xorg_bin`.
const DEFAULT_XORG_BIN: &str = "/usr/libexec/Xorg";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const DRI: &str = "/dev/dri";
const SYS_DRM: &str = "/sys/class/drm";
const NVIDIA_VERSION: &str = "/sys/module/nvidia/version";
const NVIDIA_DRM: &str = "/sys/module/nvidia_drm";
const NVIDIA_DRM_MODESET: &str = "/sys/module/nvidia_drm/parameters/modeset";
const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";
/// `PulseAudio` servers a session's audio can come from.
const PULSE_SERVERS: &[&str] = &["/usr/bin/pipewire-pulse", "/usr/bin/pulseaudio"];

fn executable(path: &str) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// What an existing `/etc/arcen/pier.json` says about the desktop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingConfig {
    /// `capture.provider`, when set.
    pub preference: Option<ProviderPreference>,
    /// `platform.session.xorg_bin`, when set.
    pub xorg_bin: Option<String>,
    /// `capture.wayland_compositor_prefix`, when set: the side-by-side
    /// GNOME the Pier's Wayland sessions run, probed instead of `/usr`.
    pub compositor_prefix: Option<CompositorPrefix>,
}

/// Reads the keys the provider decision depends on.
///
/// # Errors
///
/// The file is not JSON, `capture.provider` is not a known value or
/// `capture.wayland_compositor_prefix` is malformed: the Pier would refuse
/// it as well.
pub fn parse_existing_config(text: &str) -> Result<ExistingConfig, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("not valid JSON: {error}"))?;
    let preference = match value.pointer("/capture/provider") {
        None | Some(serde_json::Value::Null) => None,
        Some(provider) => Some(
            serde_json::from_value::<ProviderPreference>(provider.clone()).map_err(|_| {
                format!("capture.provider {provider} is not \"auto\", \"xorg\" or \"wayland\"")
            })?,
        ),
    };
    let compositor_prefix = match value.pointer("/capture/wayland_compositor_prefix") {
        None | Some(serde_json::Value::Null) => None,
        Some(prefix) => Some(
            prefix
                .as_str()
                .ok_or_else(|| "capture.wayland_compositor_prefix is not a string".to_owned())
                .and_then(|prefix| {
                    CompositorPrefix::parse(prefix)
                        .map_err(|error| format!("capture.wayland_compositor_prefix: {error}"))
                })?,
        ),
    };
    Ok(ExistingConfig {
        preference,
        xorg_bin: value
            .pointer("/platform/session/xorg_bin")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        compositor_prefix,
    })
}

/// The installed Xorg server, by the Pier's rule: the configured path, then
/// the well-known locations.
fn xorg_server(configured: Option<&str>) -> Option<String> {
    std::iter::once(configured.unwrap_or(DEFAULT_XORG_BIN))
        .chain(WELL_KNOWN_XORG_SERVERS.iter().copied())
        .find(|path| executable(path))
        .map(str::to_owned)
}

/// Runs `program args`, returning its standard output, or `None` when it
/// cannot start or outlives `timeout`.
fn bounded_stdout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    bounded_stdout_as(program, args, timeout, false)
}

/// `nobody` / `nogroup` (the kernel's overflow id).
const NOBODY: u32 = 65_534;

/// Runs `program` with a bound; with `unprivileged`, as `nobody` when the
/// installer is root (a side-by-side prefix's probes never need root).
fn bounded_stdout_as(
    program: &str,
    args: &[&str],
    timeout: Duration,
    unprivileged: bool,
) -> Option<String> {
    let mut command = Command::new(program);
    if unprivileged && rustix::process::geteuid().is_root() {
        std::os::unix::process::CommandExt::uid(&mut command, NOBODY);
        std::os::unix::process::CommandExt::gid(&mut command, NOBODY);
        command.env("HOME", "/");
    }
    let mut child = command
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    reader.join().ok()
}

/// The name of group `gid` in `/etc/group` text.
#[must_use]
pub fn group_name(groups: &str, gid: u32) -> Option<String> {
    groups.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let id = fields.nth(1)?.parse::<u32>().ok()?;
        (id == gid).then(|| name.to_owned())
    })
}

fn render_nodes() -> Vec<RenderNode> {
    let Ok(entries) = fs::read_dir(DRI) else {
        return Vec::new();
    };
    let groups = fs::read_to_string("/etc/group").unwrap_or_default();
    let mut nodes = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("renderD") {
                return None;
            }
            let device = Path::new(SYS_DRM).join(&name).join("device");
            let metadata = entry.metadata().ok();
            Some(RenderNode {
                path: format!("{DRI}/{name}"),
                world_accessible: metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.permissions().mode() & 0o006 == 0o006),
                mode: metadata
                    .as_ref()
                    .map(|metadata| metadata.permissions().mode() & 0o7777),
                group: metadata.as_ref().map(|metadata| {
                    group_name(&groups, metadata.gid())
                        .unwrap_or_else(|| metadata.gid().to_string())
                }),
                pci_address: fs::read_link(&device).ok().and_then(|target| {
                    target
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                }),
                vendor_id: fs::read_to_string(device.join("vendor"))
                    .ok()
                    .and_then(|text| {
                        u16::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
                    }),
            })
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.path.cmp(&right.path));
    nodes
}

fn parse_kernel_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "Y" | "y" | "1" => Some(true),
        "N" | "n" | "0" => Some(false),
        _ => None,
    }
}

/// The loaded NVIDIA driver, as the Pier's qualification record names it.
fn gpu_driver() -> Option<GpuDriver> {
    let version = fs::read_to_string(NVIDIA_VERSION).ok()?;
    let version = version.trim();
    let drm = Path::new(NVIDIA_DRM).is_dir();
    (!version.is_empty()).then(|| GpuDriver {
        vendor: "nvidia".to_owned(),
        version: version.to_owned(),
        drm,
        modeset: drm
            .then(|| fs::read_to_string(NVIDIA_DRM_MODESET).ok())
            .flatten()
            .as_deref()
            .and_then(parse_kernel_bool),
    })
}

fn kernel_header(release: &str) -> Option<String> {
    [
        format!("/usr/src/kernels/{release}/include/drm/drm_mode_config.h"),
        format!("/lib/modules/{release}/build/include/drm/drm_mode_config.h"),
    ]
    .iter()
    .find_map(|path| fs::read_to_string(path).ok())
}

/// [`PrefixInspector`] over this host's filesystem.
struct HostPrefixInspector;

impl PrefixInspector for HostPrefixInspector {
    fn inspect(&self, path: &str) -> PathOwnership {
        match fs::symlink_metadata(path) {
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
        fs::read_dir(path)
            .ok()?
            .map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect()
    }

    fn resolve(&self, path: &str) -> Option<String> {
        fs::canonicalize(path)
            .ok()?
            .into_os_string()
            .into_string()
            .ok()
    }
}

/// Why the existing configuration's compositor prefix may not be used, as
/// the Pier would refuse it at start (shared `verify_prefix`); `None` for
/// `/usr` or a prefix that passes. Nothing in a refused prefix is run.
#[must_use]
pub fn compositor_prefix_refusal(existing: &ExistingConfig) -> Option<String> {
    let prefix = existing.compositor_prefix.as_ref()?;
    verify_prefix(prefix, &HostPrefixInspector)
        .err()
        .map(|error| format!("capture.wayland_compositor_prefix {prefix}: {error}"))
}

/// Gathers every fact the plan needs from this machine. Read-only: it runs
/// only `--help`/`--version` of the compositor and `PipeWire`, and a
/// side-by-side prefix's compositor only after the whole prefix passed
/// [`compositor_prefix_refusal`], as `nobody`. A refused prefix contributes
/// no compositor.
pub fn gather_host_facts(existing: &ExistingConfig, wayland_built: bool) -> InstallHostFacts {
    let prefix = existing.compositor_prefix.clone().unwrap_or_default();
    let prefix_usable = compositor_prefix_refusal(existing).is_none();
    let unprivileged = !prefix.is_system();
    let compositors = WAYLAND_HOST_COMPOSITORS
        .into_iter()
        .filter(|_| prefix_usable)
        .filter(|compositor| executable(&prefix.binary(*compositor)))
        .filter_map(|compositor| {
            let help = bounded_stdout_as(
                &prefix.binary(compositor),
                &["--help"],
                PROBE_TIMEOUT,
                unprivileged,
            )?;
            let (headless, virtual_monitor) = parse_compositor_help(&help);
            Some(CompositorProbe {
                compositor,
                headless,
                virtual_monitor,
            })
        })
        .collect::<Vec<_>>();
    let render_nodes = render_nodes();
    let probe = WaylandHostProbe {
        compositors,
        logind: Path::new(LOGIND_SEATS_DIR).is_dir(),
        dbus_daemon: executable(DBUS_DAEMON_BINARY),
        pipewire: executable(PIPEWIRE_BINARY),
        session_manager: executable(WIREPLUMBER_BINARY),
        render_node: arcen_session::install_provider::render_node_fact(&render_nodes),
        ..WaylandHostProbe::default()
    };
    let compositor_version = probe.compositor().and_then(|compositor| {
        bounded_stdout_as(
            &prefix.binary(compositor),
            &["--version"],
            PROBE_TIMEOUT,
            unprivileged,
        )
        .as_deref()
        .and_then(parse_compositor_version)
    });
    let pipewire_version = probe
        .pipewire
        .then(|| bounded_stdout(PIPEWIRE_BINARY, &["--version"], PROBE_TIMEOUT))
        .flatten()
        .as_deref()
        .and_then(parse_pipewire_version);
    let kernel_release = fs::read_to_string(KERNEL_RELEASE)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let header = kernel_header(&kernel_release);
    InstallHostFacts {
        preference: existing.preference.unwrap_or_default(),
        preference_source: if existing.preference.is_some() {
            PreferenceSource::ExistingConfig
        } else {
            PreferenceSource::Default
        },
        xorg_server: xorg_server(existing.xorg_bin.as_deref()),
        wayland_built,
        probe,
        compositor_version,
        pipewire_version,
        pulse_server: PULSE_SERVERS
            .iter()
            .find(|path| executable(path))
            .map(|path| (*path).to_owned()),
        render_nodes,
        gpu_driver: gpu_driver(),
        kernel_fb_create_format_info: kernel_fb_create_format_info(
            &kernel_release,
            header.as_deref(),
        ),
        kernel_release,
    }
}

/// SELinux mode and whether Arcen's policy module is loaded, for the
/// summary. Read-only; "unknown" when `semodule -l` cannot answer (not
/// root, or the policy store is locked).
#[must_use]
pub fn selinux_summary() -> String {
    let mode = match fs::read_to_string(super::SELINUXFS_ENFORCE) {
        Ok(value) if value.trim() == "1" => "enforcing",
        Ok(_) => "permissive",
        Err(_) => return "disabled".to_owned(),
    };
    let semodule = Path::new(super::SEMODULE);
    let module = if semodule.exists() {
        match super::selinux_module_loaded(semodule) {
            Ok(true) => "loaded".to_owned(),
            Ok(false) => "not loaded".to_owned(),
            Err(error) => format!("unknown ({error})"),
        }
    } else {
        format!("unknown ({} is not installed)", semodule.display())
    };
    format!("{mode}; Arcen policy module {module}")
}

/// Whether the embedded Pier runs on this host, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PierRuntime {
    /// It starts; whether it contains the headless Wayland provider.
    Runs {
        /// `arcen-pier wayland --help` is available.
        wayland_built: bool,
    },
    /// The dynamic loader refused it: what it could not find.
    LoaderRefused(Vec<String>),
    /// It could not be run from memory here (for example a kernel that
    /// forbids executing memfd files); checked after installation instead.
    Unchecked(String),
}

/// The embedded Pier, held in an anonymous memory file so it can be run
/// before (or without) anything being written to disk.
struct MemoryPier {
    file: OwnedFd,
}

impl MemoryPier {
    #[cfg(target_os = "linux")]
    fn create() -> Result<OwnedFd, String> {
        rustix::fs::memfd_create("arcen-pier-preflight", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|error| format!("memfd_create: {error}"))
    }

    #[cfg(not(target_os = "linux"))]
    fn create() -> Result<OwnedFd, String> {
        Err("memory files need Linux".to_owned())
    }

    fn load(bytes: &[u8]) -> Result<Self, String> {
        let file = Self::create()?;
        let mut writer = fs::File::from(file);
        writer
            .write_all(bytes)
            .map_err(|error| format!("write the embedded Pier to memory: {error}"))?;
        Ok(Self {
            file: OwnedFd::from(writer),
        })
    }

    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        Command::new(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
            .args(args)
            .stdin(Stdio::null())
            .output()
    }
}

/// Libraries the loader reports missing, from its standard error.
#[must_use]
pub fn loader_failures(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| {
            if let Some((_, rest)) = line.split_once("error while loading shared libraries: ") {
                return Some(rest.split(':').next().unwrap_or(rest).trim().to_owned());
            }
            let start = line.find("version `")?;
            let rest = &line[start + "version `".len()..];
            let version = rest.split('\'').next()?;
            line.contains("not found")
                .then(|| format!("{version} (a newer C library than this host has)"))
        })
        .collect()
}

/// Checks that the embedded Pier starts here, and whether it has Wayland.
pub fn check_pier_runtime(bytes: &[u8]) -> PierRuntime {
    let pier = match MemoryPier::load(bytes) {
        Ok(pier) => pier,
        Err(error) => return PierRuntime::Unchecked(error),
    };
    let version = match pier.run(&["--version"]) {
        Ok(output) => output,
        Err(error) => return PierRuntime::Unchecked(format!("run from memory: {error}")),
    };
    if !version.status.success() {
        let stderr = String::from_utf8_lossy(&version.stderr);
        let missing = loader_failures(&stderr);
        return if missing.is_empty() {
            PierRuntime::Unchecked(format!(
                "`arcen-pier --version` exited with {}: {}",
                version.status,
                stderr.trim()
            ))
        } else {
            PierRuntime::LoaderRefused(missing)
        };
    }
    let wayland_built = pier
        .run(&["wayland", "--help"])
        .is_ok_and(|output| output.status.success());
    PierRuntime::Runs { wayland_built }
}

/// Explains a loader refusal.
#[must_use]
pub fn loader_refusal_message(missing: &[String]) -> String {
    let mut text =
        String::from("the Pier cannot start on this host; the dynamic loader cannot find:\n");
    for library in missing {
        text.push_str(&format!("  {library}\n"));
    }
    text.push_str(
        "\nThe Pier is one binary for every Linux host, X11 and Wayland alike, so it needs \
         these libraries even where only one desktop is used (the PipeWire client library, \
         libpipewire-0.3.so.0, included). Install the packages that provide them (for example \
         `dnf provides '*/<library>'` or `apt-file search <library>`) and run the installer \
         again. Nothing was installed.",
    );
    text
}

/// Validates `--qualify-output WIDTHxHEIGHT@HZ`.
#[must_use]
pub fn valid_qualify_output(value: &str) -> bool {
    let Some((size, refresh)) = value.split_once('@') else {
        return false;
    };
    let Some((width, height)) = size.split_once('x') else {
        return false;
    };
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse::<u32>().ok())
            .flatten()
    };
    match (number(width), number(height), number(refresh)) {
        (Some(width), Some(height), Some(refresh)) => {
            VirtualOutputRequest::new(width, height, refresh).is_ok()
        }
        _ => false,
    }
}

/// What `arcen-pier wayland qualify --write` reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationRun {
    /// Exit status 0: Wayland is servable with the record it wrote.
    pub passed: bool,
    /// Exit code, when it exited.
    pub exit_code: Option<i32>,
    /// The lines naming what failed.
    pub failures: Vec<String>,
}

/// Keeps the record's JSON (already in the log) off the terminal.
#[derive(Debug, Default)]
pub struct RecordEcho {
    in_record: bool,
}

impl RecordEcho {
    /// Whether to show `line` on the terminal.
    pub fn show(&mut self, line: &str) -> bool {
        if self.in_record {
            if line == "}" {
                self.in_record = false;
            }
            return false;
        }
        if line == "{" {
            self.in_record = true;
            return false;
        }
        true
    }
}

/// The qualification output lines that say what failed.
#[must_use]
pub fn qualification_failures(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| {
            line.starts_with("FAIL ")
                || line.starts_with("Wayland on this host: refused")
                || line.starts_with("qualify: ")
                || (line.starts_with("Start-up selection") && line.contains("refused"))
        })
        .map(str::to_owned)
        .collect()
}

enum Stream {
    Out(String),
    Err(String),
}

/// Runs the installed Pier's qualification, echoing its progress and
/// logging everything.
///
/// # Errors
///
/// When the Pier cannot be started at all.
pub fn run_qualification(
    pier: &Path,
    outputs: &[String],
    log: &mut InstallLog,
) -> Result<QualificationRun, String> {
    let mut args = vec!["wayland", "qualify", "--write"];
    for output in outputs {
        args.extend(["--output", output.as_str()]);
    }
    log.note(format!("running: {} {}", pier.display(), args.join(" ")));
    let mut child = Command::new(pier)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start {}: {error}", pier.display()))?;
    let (sender, receiver) = mpsc::channel();
    let stdout = child.stdout.take().map(|stdout| {
        let sender = sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = sender.send(Stream::Out(line));
            }
        })
    });
    let stderr = child.stderr.take().map(|stderr| {
        let sender = sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = sender.send(Stream::Err(line));
            }
        })
    });
    drop(sender);
    let mut echo = RecordEcho::default();
    let mut seen = Vec::new();
    for message in receiver {
        match message {
            Stream::Out(line) => {
                if echo.show(&line) {
                    println!("  {line}");
                } else if line == "{" {
                    println!("  (the full qualification record is in {INSTALL_LOG})");
                }
                log.record(format!("qualify: {line}"));
                seen.push(line);
            }
            Stream::Err(line) => {
                eprintln!("  {line}");
                log.record(format!("qualify (stderr): {line}"));
                seen.push(line);
            }
        }
    }
    for reader in [stdout, stderr].into_iter().flatten() {
        let _ = reader.join();
    }
    let status = child
        .wait()
        .map_err(|error| format!("wait for qualification: {error}"))?;
    log.record(format!("qualify exit status: {status}"));
    Ok(QualificationRun {
        passed: status.success(),
        exit_code: status.code(),
        failures: qualification_failures(&seen),
    })
}

/// Lines to print and to append to [`INSTALL_LOG`].
#[derive(Debug, Default)]
pub struct InstallLog {
    lines: Vec<String>,
}

impl InstallLog {
    /// Prints `line` and records it.
    pub fn note(&mut self, line: impl Into<String>) {
        let line = line.into();
        println!("{line}");
        self.lines.push(line);
    }

    /// Records `line` without printing it.
    pub fn record(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    /// Appends the recorded lines under a dated header to `path`, when its
    /// directory exists (it is never created for the log alone).
    ///
    /// # Errors
    ///
    /// The file cannot be opened or written.
    pub fn append_to(&mut self, path: &Path, action: &str) -> Result<(), String> {
        if self.lines.is_empty() || !path.parent().is_some_and(Path::is_dir) {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        let mut text = format!(
            "=== install-arcen-pier {} {action} {} ===\n",
            env!("CARGO_PKG_VERSION"),
            utc_now()
        );
        for line in self.lines.drain(..) {
            text.push_str(&line);
            text.push('\n');
        }
        file.write_all(text.as_bytes())
            .map_err(|error| format!("write {}: {error}", path.display()))
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for the current time.
fn utc_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    utc_timestamp(seconds)
}

/// Civil UTC time for Unix seconds (Howard Hinnant's `civil_from_days`).
#[must_use]
pub fn utc_timestamp(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let of_day = seconds % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

/// The local `/etc/passwd` entry `name`: its `GECOS` field.
#[must_use]
pub fn local_account_gecos(passwd: &str, name: &str) -> Option<String> {
    passwd.lines().find_map(|line| {
        let fields = line.split(':').collect::<Vec<_>>();
        (fields.len() >= 7 && fields[0] == name).then(|| fields[4].to_owned())
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_refused_compositor_prefix_is_reported_and_never_executed() {
        // Under the temporary directory (world-writable): refused whoever
        // owns the prefix itself.
        let root =
            std::env::temp_dir().join(format!("arcen-prefix-refused-{}", std::process::id()));
        let marker = root.join("ran");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["gnome-shell", "mutter"] {
            let path = bin.join(name);
            fs::write(&path, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let existing = ExistingConfig {
            compositor_prefix: Some(CompositorPrefix::parse(root.to_str().unwrap()).unwrap()),
            ..ExistingConfig::default()
        };
        let refusal = compositor_prefix_refusal(&existing).expect("refused");
        assert!(
            refusal.contains("capture.wayland_compositor_prefix"),
            "{refusal}"
        );
        assert!(
            refusal.contains("writable") || refusal.contains("not root"),
            "{refusal}"
        );
        let facts = gather_host_facts(&existing, true);
        assert!(facts.probe.compositors.is_empty());
        assert_eq!(facts.compositor_version, None);
        assert!(!marker.exists(), "a refused prefix's binary was run");
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(compositor_prefix_refusal(&ExistingConfig::default()), None);
    }

    #[test]
    fn existing_config_supplies_the_override_and_the_xorg_path() {
        assert_eq!(
            parse_existing_config(r#"{"capture":{}}"#).unwrap(),
            ExistingConfig::default()
        );
        assert_eq!(
            parse_existing_config(
                r#"{"capture":{"provider":"xorg"},"platform":{"session":{"xorg_bin":"/opt/X"}}}"#
            )
            .unwrap(),
            ExistingConfig {
                preference: Some(ProviderPreference::Xorg),
                xorg_bin: Some("/opt/X".to_owned()),
                compositor_prefix: None,
            }
        );
        assert_eq!(
            parse_existing_config(
                r#"{"capture":{"wayland_compositor_prefix":"/opt/arcen-gnome/51"}}"#
            )
            .unwrap()
            .compositor_prefix
            .map(|prefix| prefix
                .binary(arcen_session::wayland_session::HeadlessCompositor::GnomeShell)),
            Some("/opt/arcen-gnome/51/bin/gnome-shell".to_owned())
        );
        assert!(
            parse_existing_config(r#"{"capture":{"wayland_compositor_prefix":"opt"}}"#).is_err()
        );
        assert_eq!(
            parse_existing_config(r#"{"capture":{"provider":"wayland"}}"#)
                .unwrap()
                .preference,
            Some(ProviderPreference::Wayland)
        );
        assert!(parse_existing_config(r#"{"capture":{"provider":"weston"}}"#).is_err());
        assert!(parse_existing_config("{").is_err());
    }

    #[test]
    fn the_packaged_template_defaults_to_auto() {
        // Absent means `auto`; the key stays out of the shared common
        // sections because Windows and macOS refuse it.
        let template = parse_existing_config(include_str!("../../arcen-pier.json")).unwrap();
        assert_eq!(template.preference, None);
        assert_eq!(
            template.preference.unwrap_or_default(),
            ProviderPreference::Auto
        );
        assert_eq!(template.xorg_bin.as_deref(), Some(DEFAULT_XORG_BIN));
    }

    #[test]
    fn loader_failures_name_the_missing_library_or_symbol_version() {
        let stderr = "/proc/self/fd/3: error while loading shared libraries: \
                      libpipewire-0.3.so.0: cannot open shared object file: No such file or \
                      directory\n";
        assert_eq!(loader_failures(stderr), ["libpipewire-0.3.so.0"]);
        let glibc = "/proc/self/fd/3: /lib64/libc.so.6: version `GLIBC_2.38' not found \
                     (required by /proc/self/fd/3)\n";
        assert_eq!(
            loader_failures(glibc),
            ["GLIBC_2.38 (a newer C library than this host has)"]
        );
        assert!(loader_failures("configuration failed: nope").is_empty());
        let message = loader_refusal_message(&["libpipewire-0.3.so.0".to_owned()]);
        assert!(message.contains("libpipewire-0.3.so.0"));
        assert!(message.contains("Nothing was installed"));
    }

    #[test]
    fn qualify_outputs_are_validated_like_the_pier() {
        assert!(valid_qualify_output("1920x1080@60"));
        assert!(valid_qualify_output("2560x1440@120"));
        for bad in [
            "1920x1080",
            "x1080@60",
            "1920x1080@",
            "99999x10@60",
            "1920x1080@0",
            "a@b",
        ] {
            assert!(!valid_qualify_output(bad), "{bad}");
        }
    }

    #[test]
    fn the_record_json_stays_off_the_terminal() {
        let mut echo = RecordEcho::default();
        let shown = [
            "ok   host: x",
            "",
            "{",
            "  \"schema_version\": 2,",
            "  \"facts\": {",
            "  }",
            "}",
            "Recorded in /var/lib/arcen/wayland-qualification.json.",
        ]
        .into_iter()
        .filter(|line| echo.show(line))
        .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [
                "ok   host: x",
                "",
                "Recorded in /var/lib/arcen/wayland-qualification.json."
            ]
        );
    }

    #[test]
    fn qualification_failures_are_the_fail_and_refusal_lines() {
        let lines = [
            "ok   host: rocky 10.2",
            "FAIL open_session: launcher refused: avc denied",
            "Supported with this record:",
            "Wayland on this host: refused: no Xorg server is installed, and ...",
            "Start-up selection with this record (capture.provider = auto): refused: x",
            "Start-up selection with this record (capture.provider = auto): Wayland (wayland_qualified).",
            "qualify: arcen-pier.service is active",
        ]
        .map(str::to_owned);
        assert_eq!(
            qualification_failures(&lines),
            [
                "FAIL open_session: launcher refused: avc denied",
                "Wayland on this host: refused: no Xorg server is installed, and ...",
                "Start-up selection with this record (capture.provider = auto): refused: x",
                "qualify: arcen-pier.service is active",
            ]
        );
    }

    #[test]
    fn group_ids_resolve_to_names() {
        let groups = "root:x:0:\nvideo:x:39:\nrender:x:105:alice\n";
        assert_eq!(group_name(groups, 105).as_deref(), Some("render"));
        assert_eq!(group_name(groups, 0).as_deref(), Some("root"));
        assert_eq!(group_name(groups, 7), None);
    }

    #[test]
    fn timestamps_are_civil_utc() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_timestamp(1_792_272_921), "2026-10-17T21:35:21Z");
    }

    #[test]
    fn only_the_named_local_account_is_found() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      arcen-wlq:x:1001:1001:Arcen Wayland qualification (temporary):/home/arcen-wlq:/sbin/nologin\n";
        assert_eq!(
            local_account_gecos(passwd, "arcen-wlq").as_deref(),
            Some("Arcen Wayland qualification (temporary)")
        );
        assert_eq!(local_account_gecos(passwd, "arcen"), None);
    }

    #[test]
    fn the_install_log_is_appended_with_a_header_only_where_its_directory_exists() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".arcen-linux-installer-tests")
            .join(format!("log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let mut log = InstallLog::default();
        log.record("first");
        log.append_to(&directory.join("install.log"), "install")
            .unwrap();
        assert!(!directory.exists(), "the log never creates its directory");
        fs::create_dir_all(&directory).unwrap();
        log.append_to(&directory.join("install.log"), "install")
            .unwrap();
        log.record("second");
        log.append_to(&directory.join("install.log"), "uninstall")
            .unwrap();
        let text = fs::read_to_string(directory.join("install.log")).unwrap();
        assert!(text.contains(" install "), "{text}");
        assert!(text.contains("first\n"), "{text}");
        assert!(text.contains(" uninstall "), "{text}");
        assert!(text.ends_with("second\n"), "{text}");
        let mode = fs::metadata(directory.join("install.log"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
        let _ = fs::remove_dir_all(&directory);
    }
}
