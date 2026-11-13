// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The operating-system half of `arcen_usb_bridge::bridge_seat`: writes and
//! removes the volatile per-port udev rule, finds and verifies the input
//! nodes of a bridged device, and sweeps rules a dead helper left behind.
//!
//! The rule is installed by the privileged helper between "the port's sysfs
//! path is known" and "a device is connected to it", for every virtual host
//! controller backend, so a bridged device is never unclaimed, not even for
//! one udev event. For a dedicated Xorg desktop it also reads which X
//! servers run and whether each keeps the device out ([`x11_isolation`]).

use arcen_usb_bridge::bridge_seat::{
    self, BridgeSeat, BridgedInputNode, BridgedNodeRole, RULE_DIRECTORY,
};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The comment line naming the helper that owns a rule.
const OWNER_LINE: &str = "# arcen-owner-pid: ";
/// Bound on each `udevadm` call.
const UDEVADM_TIMEOUT: Duration = Duration::from_secs(10);
/// How long nodes must stay unchanged before they are taken as complete.
const NODES_STABLE_FOR: Duration = Duration::from_millis(600);

/// One installed rule; dropping it removes the rule and reloads udev.
#[derive(Debug)]
pub struct SeatRule {
    path: PathBuf,
    reload: bool,
}

impl SeatRule {
    /// Installs the rule for the port at `prefix` in `/run/udev/rules.d` and
    /// reloads udev's rules, so it is in force before the port is connected.
    ///
    /// # Errors
    ///
    /// When `prefix` or `seat` is invalid, the rule cannot be written, or
    /// `udevadm control --reload` fails; nothing is left behind.
    pub fn install(prefix: &str, seat: &BridgeSeat) -> io::Result<Self> {
        Self::install_in(Path::new(RULE_DIRECTORY), prefix, seat, true, process_alive)
    }

    fn install_in(
        directory: &Path,
        prefix: &str,
        seat: &BridgeSeat,
        reload: bool,
        alive: impl Fn(u32) -> bool,
    ) -> io::Result<Self> {
        let text = bridge_seat::udev_rule(prefix, seat)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        fs::create_dir_all(directory)?;
        // A helper killed without cleanup left its rule for this port: its
        // seat and links would apply to the new device too. It goes first; a
        // live bridge's rule on this port refuses the install.
        let (_, live) = clear_port_rules_in(directory, prefix, &alive);
        if live {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("a live bridge already owns the seat rule for {prefix}"),
            ));
        }
        let name = seat.rule_file_name();
        let path = directory.join(&name);
        let temporary = directory.join(format!(".{name}.{}", std::process::id()));
        let _ = fs::remove_file(&temporary);
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&temporary)?;
            writeln!(file, "{OWNER_LINE}{}", std::process::id())?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        let rule = Self { path, reload };
        if reload {
            udevadm(&["control", "--reload"])?;
        }
        Ok(rule)
    }

    /// The rule file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SeatRule {
    fn drop(&mut self) {
        let removed = fs::remove_file(&self.path).is_ok();
        if removed && self.reload {
            let _ = udevadm(&["control", "--reload"]);
        }
    }
}

fn udevadm(arguments: &[&str]) -> io::Result<()> {
    let mut child = Command::new("udevadm")
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + UDEVADM_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "udevadm {arguments:?} failed: {status}"
                )))
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("udevadm {arguments:?} timed out"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The sysfs path of port `port` on the root hub of USB bus `bus`, which
/// must belong to a virtual host controller.
///
/// # Errors
///
/// When the bus does not exist or is not a virtual controller's.
pub fn port_prefix_for_bus(bus: i32, port: u8) -> io::Result<String> {
    port_prefix_in(Path::new("/sys"), bus, port)
}

fn port_prefix_in(sysfs: &Path, bus: i32, port: u8) -> io::Result<String> {
    let real = fs::canonicalize(sysfs.join(format!("bus/usb/devices/usb{bus}")))?;
    let root = fs::canonicalize(sysfs)?;
    let relative = real
        .strip_prefix(&root)
        .map_err(|_| io::Error::other("USB bus outside sysfs"))?;
    let prefix = format!("/{}/{bus}-{port}", relative.display());
    bridge_seat::validate_port_prefix(&prefix)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(prefix)
}

/// Waits until nothing remains under `prefix` in sysfs.
#[must_use]
pub fn wait_subtree_gone(prefix: &str, timeout: Duration) -> bool {
    let path = Path::new("/sys").join(prefix.trim_start_matches('/'));
    let deadline = Instant::now() + timeout;
    loop {
        if !path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Removes bridge rules whose owning helper is gone (a crashed or killed
/// helper, or a Pier stopped without teardown). A rule owned by a live
/// process, including another Pier's helper on this host, is kept.
pub fn sweep_stale_rules() -> Vec<String> {
    let removed = sweep_stale_rules_in(Path::new(RULE_DIRECTORY), |pid| {
        Path::new("/proc").join(pid.to_string()).exists()
    });
    if !removed.is_empty() {
        let _ = udevadm(&["control", "--reload"]);
    }
    removed
}

fn process_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn rule_owner(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.strip_prefix(OWNER_LINE))
        .and_then(|pid| pid.trim().parse::<u32>().ok())
}

/// Removes the rules naming `port` whose owner is gone; returns them and
/// whether a live owner holds one.
fn clear_port_rules_in(
    directory: &Path,
    port: &str,
    alive: &impl Fn(u32) -> bool,
) -> (Vec<String>, bool) {
    let Ok(entries) = fs::read_dir(directory) else {
        return (Vec::new(), false);
    };
    let mut removed = Vec::new();
    let mut live = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !bridge_seat::is_rule_file(&name) {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        if bridge_seat::rule_port(&text) != Some(port) {
            continue;
        }
        if rule_owner(&text).is_some_and(alive) {
            live = true;
        } else if fs::remove_file(entry.path()).is_ok() {
            removed.push(name);
        }
    }
    (removed, live)
}

fn sweep_stale_rules_in(directory: &Path, alive: impl Fn(u32) -> bool) -> Vec<String> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !bridge_seat::is_rule_file(&name) {
            continue;
        }
        let owner = fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| rule_owner(&text));
        if owner.is_some_and(&alive) {
            continue;
        }
        if fs::remove_file(entry.path()).is_ok() {
            removed.push(name);
        }
    }
    removed
}

/// Where udev and sysfs are read from (overridden in tests).
#[derive(Debug, Clone)]
struct Roots {
    sysfs: PathBuf,
    udev_data: PathBuf,
    dev: PathBuf,
}

impl Roots {
    fn system() -> Self {
        Self {
            sysfs: PathBuf::from("/sys"),
            udev_data: PathBuf::from("/run/udev/data"),
            dev: PathBuf::from("/dev"),
        }
    }
}

/// Verifies one node fact against the system: its sysfs path is under the
/// port, it is the event node it claims, and udev applied the bridge rule
/// (`bridge_seat::verify_seat`: no `uaccess`; on its native seat, or tagged
/// for its X11 desktop alone).
///
/// # Errors
///
/// A bounded reason.
pub fn verify_node(node: &BridgedInputNode, prefix: &str, seat: &BridgeSeat) -> Result<(), String> {
    verify_node_in(&Roots::system(), node, prefix, seat)
}

fn verify_node_in(
    roots: &Roots,
    node: &BridgedInputNode,
    prefix: &str,
    seat: &BridgeSeat,
) -> Result<(), String> {
    node.validate(prefix).map_err(|error| error.to_string())?;
    let leaf = node.devpath.rsplit('/').next().unwrap_or_default();
    let class = roots.sysfs.join("class/input").join(leaf);
    let real = fs::canonicalize(&class).map_err(|error| format!("{leaf}: {error}"))?;
    let expected = roots.sysfs.join(node.devpath.trim_start_matches('/'));
    let expected = fs::canonicalize(&expected).map_err(|error| format!("{leaf}: {error}"))?;
    if real != expected {
        return Err(format!("{leaf} is not at its claimed sysfs path"));
    }
    let numbers =
        fs::read_to_string(class.join("dev")).map_err(|error| format!("{leaf}: {error}"))?;
    let database = fs::read_to_string(roots.udev_data.join(format!("c{}", numbers.trim())))
        .map_err(|error| format!("{leaf} has no udev entry: {error}"))?;
    if !bridge_seat::verify_seat(&database, seat) {
        return Err(format!(
            "{leaf} does not carry the bridge rule of {}",
            seat.seat_name()
        ));
    }
    if !roots.dev.join("input").join(leaf).exists() {
        return Err(format!("{leaf} has no device node yet"));
    }
    Ok(())
}

/// Whether xf86-input-wacom is installed (the only X driver a dedicated Xorg
/// takes a bridged tablet with: libinput is told to ignore bridged nodes).
#[must_use]
pub fn wacom_x_driver_installed() -> bool {
    [
        "/usr/lib64/xorg/modules/input/wacom_drv.so",
        "/usr/lib/xorg/modules/input/wacom_drv.so",
        "/usr/lib/x86_64-linux-gnu/xorg/modules/input/wacom_drv.so",
    ]
    .iter()
    .any(|path| Path::new(path).is_file())
}

/// Finds every input event node under `prefix`, waiting (bounded) until the
/// set has been stable for a moment and holds at least one node, and
/// verifies each with [`verify_node`]. Any unverified node fails the whole
/// set: nothing is handed on.
///
/// # Errors
///
/// A bounded reason (timeout, a node off its seat).
pub fn discover_nodes(
    prefix: &str,
    seat: &BridgeSeat,
    timeout: Duration,
) -> Result<Vec<BridgedInputNode>, String> {
    discover_nodes_until(prefix, seat, timeout, &|| false)
}

/// [`discover_nodes`] that gives up as soon as `cancelled` says so (its
/// caller no longer wants the nodes).
///
/// # Errors
///
/// A bounded reason (timeout, cancelled, a node off its seat).
pub fn discover_nodes_until(
    prefix: &str,
    seat: &BridgeSeat,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<BridgedInputNode>, String> {
    let _ = udevadm(&["settle", "--timeout=5"]);
    discover_nodes_in(&Roots::system(), prefix, seat, timeout, cancelled)
}

/// The device number (`major:minor`) sysfs gives a node's input device.
///
/// # Errors
///
/// When sysfs has no such node or its number does not parse.
pub fn node_device_number(node: &BridgedInputNode) -> Result<(u64, u64), String> {
    let leaf = node.devpath.rsplit('/').next().unwrap_or_default();
    let text = fs::read_to_string(Path::new("/sys/class/input").join(leaf).join("dev"))
        .map_err(|error| format!("{leaf}: {error}"))?;
    parse_device_number(&text).ok_or_else(|| format!("{leaf}: unreadable device number"))
}

fn parse_device_number(text: &str) -> Option<(u64, u64)> {
    let (major, minor) = text.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn discover_nodes_in(
    roots: &Roots,
    prefix: &str,
    seat: &BridgeSeat,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<BridgedInputNode>, String> {
    let deadline = Instant::now() + timeout;
    let mut last: Option<(Vec<BridgedInputNode>, Instant)> = None;
    loop {
        if cancelled() {
            return Err("node discovery cancelled".to_owned());
        }
        let nodes = scan_nodes(roots, prefix)?;
        let verified = nodes
            .iter()
            .all(|node| verify_node_in(roots, node, prefix, seat).is_ok());
        let now = Instant::now();
        match &last {
            Some((previous, since))
                if *previous == nodes
                    && !nodes.is_empty()
                    && verified
                    && now.duration_since(*since) >= NODES_STABLE_FOR =>
            {
                return Ok(nodes);
            }
            Some((previous, _)) if *previous == nodes => {}
            _ => last = Some((nodes.clone(), now)),
        }
        if now >= deadline {
            if let Some(node) = nodes
                .iter()
                .find(|node| verify_node_in(roots, node, prefix, seat).is_err())
            {
                return Err(verify_node_in(roots, node, prefix, seat).unwrap_err());
            }
            return Err(format!(
                "the bridged device presented no stable input nodes within {} s",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn scan_nodes(roots: &Roots, prefix: &str) -> Result<Vec<BridgedInputNode>, String> {
    let class = roots.sysfs.join("class/input");
    let sysfs_root = fs::canonicalize(&roots.sysfs).map_err(|error| error.to_string())?;
    let mut nodes = Vec::new();
    let Ok(entries) = fs::read_dir(&class) else {
        return Ok(nodes);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let Ok(real) = fs::canonicalize(entry.path()) else {
            continue;
        };
        let Ok(relative) = real.strip_prefix(&sysfs_root) else {
            continue;
        };
        let devpath = format!("/{}", relative.display());
        if !bridge_seat::assigns(prefix, &devpath) {
            continue;
        }
        let database = fs::read_to_string(entry.path().join("dev"))
            .ok()
            .and_then(|numbers| {
                fs::read_to_string(roots.udev_data.join(format!("c{}", numbers.trim()))).ok()
            })
            .unwrap_or_default();
        if !database.lines().any(|line| line == "E:ID_INPUT=1") {
            continue;
        }
        let device_name = fs::read_to_string(entry.path().join("device/name"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        nodes.push(BridgedInputNode {
            devnode: format!("/dev/input/{name}"),
            devpath,
            role: BridgedNodeRole::from_udev_database(&database),
            name: device_name,
            kernel_driver: kernel_driver_in(&entry.path()),
        });
    }
    if nodes.len() > bridge_seat::MAX_BRIDGED_INPUT_NODES {
        return Err("the bridged device presented too many input nodes".to_owned());
    }
    nodes.sort_by(|left, right| left.devpath.cmp(&right.devpath));
    Ok(nodes)
}

fn kernel_driver_in(class_entry: &Path) -> String {
    fs::read_link(class_entry.join("device/device/driver"))
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| {
            name.len() <= 32
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
        .unwrap_or_default()
}

/// Whether the kernel's console keyboard handler binds this node's input
/// device (keyboard keys in its `capabilities/key`); unreadable counts as
/// yes.
#[must_use]
pub fn reaches_keyboard_handler(node: &BridgedInputNode) -> bool {
    let leaf = node.devpath.rsplit('/').next().unwrap_or_default();
    fs::read_to_string(
        Path::new("/sys/class/input")
            .join(leaf)
            .join("device/capabilities/key"),
    )
    .map_or(true, |bitmap| {
        bridge_seat::key_bitmap_reaches_keyboard_handler(&bitmap, usize::BITS)
    })
}

/// Proves that a bridge tagged for the dedicated Xorg desktop on `:display`
/// would reach that desktop's X server and no other
/// (`bridge_seat::x11_isolation_in_force`), from every running X server's
/// command line, configuration and start time, and the installed ignore
/// class.
///
/// # Errors
///
/// The first gap found.
pub fn x11_isolation(display: u16) -> Result<(), bridge_seat::X11IsolationGap> {
    let ignore = Path::new(bridge_seat::XORG_BRIDGE_IGNORE_PATH);
    let installed_at = fs::read_to_string(ignore)
        .ok()
        .filter(|text| bridge_seat::is_xorg_bridge_ignore_conf(text))
        .and_then(|_| fs::metadata(ignore).ok()?.modified().ok());
    let Some(installed_at) = installed_at else {
        return Err(bridge_seat::X11IsolationGap::IgnoreNotInstalled);
    };
    let servers = x_servers(Path::new("/proc"), installed_at);
    bridge_seat::x11_isolation_in_force(true, &servers, display)
}

/// Every running X server (`Xorg`/`X`) under `proc`.
fn x_servers(
    proc: &Path,
    ignore_installed_at: std::time::SystemTime,
) -> Vec<bridge_seat::XServerFacts> {
    let boot = boot_time(proc);
    let Ok(entries) = fs::read_dir(proc) else {
        return Vec::new();
    };
    let mut servers = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let path = entry.path();
        let comm = fs::read_to_string(path.join("comm")).unwrap_or_default();
        if !matches!(comm.trim(), "Xorg" | "X") {
            continue;
        }
        let Ok(cmdline) = fs::read(path.join("cmdline")) else {
            continue;
        };
        let arguments: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect();
        let started = fs::read_to_string(path.join("stat"))
            .ok()
            .and_then(|stat| process_start(&stat, boot?));
        servers.push(x_server_facts(
            pid,
            &arguments,
            started,
            ignore_installed_at,
        ));
    }
    servers
}

/// The facts of one X server from its arguments and start time.
fn x_server_facts(
    pid: u32,
    arguments: &[String],
    started: Option<std::time::SystemTime>,
    ignore_installed_at: std::time::SystemTime,
) -> bridge_seat::XServerFacts {
    let display = arguments
        .iter()
        .skip(1)
        .find_map(|argument| crate::usb_bridge::x11_display_number(argument));
    let option = |name: &str| {
        arguments
            .iter()
            .position(|argument| argument == name)
            .and_then(|index| arguments.get(index + 1))
    };
    let config = option("-config")
        .filter(|path| path.starts_with('/'))
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    let configdir = arguments.iter().any(|argument| argument == "-configdir");
    bridge_seat::XServerFacts {
        pid,
        display,
        admits_bridge: bridge_seat::admitted_bridge_display(&config),
        ignores_foreign_input: config.contains(arcen_input::virtual_device::FOREIGN_INPUT_CLASS),
        loaded_bridge_ignore: !configdir
            && started.is_some_and(|started| started >= ignore_installed_at),
    }
}

/// The boot time from `/proc/stat`'s `btime`.
fn boot_time(proc: &Path) -> Option<std::time::SystemTime> {
    let stat = fs::read_to_string(proc.join("stat")).ok()?;
    let seconds = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
}

/// A process's start time from its `/proc/<pid>/stat` (field 22, in clock
/// ticks since boot), rounded down to the second.
fn process_start(stat: &str, boot: std::time::SystemTime) -> Option<std::time::SystemTime> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    let ticks = after_name.split_whitespace().nth(19)?.parse::<u64>().ok()?;
    // SAFETY: sysconf has no preconditions and only reads a constant.
    let per_second = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) };
    let per_second = u64::try_from(per_second).ok().filter(|value| *value > 0)?;
    Some(boot + Duration::from_secs(ticks / per_second))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const PREFIX: &str = "/devices/platform/vhci_hcd.0/usb3/3-1";

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir();
        let base = fs::canonicalize(&base).unwrap_or(base);
        let path = base.join(format!("arcen-bridge-seat-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn x11(display: u16, byte: u8) -> BridgeSeat {
        BridgeSeat::x11_session(display, [byte; 8]).unwrap()
    }

    fn seat() -> BridgeSeat {
        BridgeSeat::x11_session(58, [1, 2, 3, 4, 5, 6, 7, 8]).unwrap()
    }

    #[test]
    fn a_rule_is_installed_atomically_and_removed_on_drop() {
        let dir = scratch("rule");
        let rule = SeatRule::install_in(&dir, PREFIX, &seat(), false, |_| false).unwrap();
        let text = fs::read_to_string(rule.path()).unwrap();
        assert!(text.starts_with(OWNER_LINE));
        assert!(text.contains("arcen_bridge,arcen_bridge_58"));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temporary left");
        let path = rule.path().to_owned();
        drop(rule);
        assert!(!path.exists());
        assert!(
            SeatRule::install_in(&dir, "/devices/pci0000:00/usb1/1-1", &seat(), false, |_| {
                false
            })
            .is_err()
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// Review (hardening): a killed helper's rule for the same port is
    /// removed before a new bridge's rule goes in; another port's stays for
    /// the start sweep; a live bridge's rule on the port refuses the install.
    #[test]
    fn a_new_rule_first_removes_a_dead_bridges_rule_for_the_same_port() {
        let dir = scratch("port");
        let old = x11(58, 0x0a);
        let other_port = x11(58, 0x0b);
        let write = |seat: &BridgeSeat, port: &str, owner: u32| {
            let text = bridge_seat::udev_rule(port, seat).unwrap();
            fs::write(
                dir.join(seat.rule_file_name()),
                format!("{OWNER_LINE}{owner}\n{text}"),
            )
            .unwrap();
        };
        write(&old, PREFIX, 4_000_001);
        write(
            &other_port,
            "/devices/platform/vhci_hcd.0/usb3/3-2",
            4_000_002,
        );
        let rule = SeatRule::install_in(&dir, PREFIX, &seat(), false, |_| false).unwrap();
        assert!(
            !dir.join(old.rule_file_name()).exists(),
            "the dead rule for this port is gone"
        );
        assert!(dir.join(other_port.rule_file_name()).exists());
        assert!(rule.path().exists());
        drop(rule);
        // A live owner's rule on the same port refuses a second bridge.
        write(&old, PREFIX, 4_000_003);
        let refused = SeatRule::install_in(&dir, PREFIX, &seat(), false, |pid| pid == 4_000_003);
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::AddrInUse);
        assert!(dir.join(old.rule_file_name()).exists());
        assert!(!dir.join(seat().rule_file_name()).exists());
    }

    #[test]
    fn the_sweep_removes_only_rules_of_dead_owners() {
        let dir = scratch("sweep");
        let live = SeatRule::install_in(&dir, PREFIX, &seat(), false, |_| false).unwrap();
        let dead_seat = x11(58, 9);
        fs::write(
            dir.join(dead_seat.rule_file_name()),
            format!("{OWNER_LINE}4000000\n# rule\n"),
        )
        .unwrap();
        fs::write(
            dir.join("72-arcen-usb-bridge-usb-ownerless.rules"),
            "# no owner\n",
        )
        .unwrap();
        fs::write(dir.join("61-other.rules"), "# not ours\n").unwrap();
        let own = std::process::id();
        let mut removed = sweep_stale_rules_in(&dir, |pid| pid == own);
        removed.sort();
        assert_eq!(
            removed,
            vec![
                "72-arcen-usb-bridge-usb-ownerless.rules".to_owned(),
                "72-arcen-usb-bridge-x11-58-0909090909090909.rules".to_owned(),
            ]
        );
        assert!(live.path().exists());
        assert!(dir.join("61-other.rules").exists());
    }

    /// A fake sysfs/udev tree: one node under the port on the bridge seat,
    /// one node on the console seat elsewhere.
    fn tree(name: &str, database: &str) -> Roots {
        let root = scratch(name);
        let sysfs = root.join("sys");
        let udev_data = root.join("udev");
        let dev = root.join("dev");
        fs::create_dir_all(&udev_data).unwrap();
        fs::create_dir_all(dev.join("input")).unwrap();
        for leaf in ["event2", "event9"] {
            fs::write(dev.join("input").join(leaf), "").unwrap();
        }
        let add = |devpath: &str, minor: u32, db: &str, device_name: &str| {
            let node = sysfs.join(devpath.trim_start_matches('/'));
            fs::create_dir_all(node.join("device")).unwrap();
            fs::write(node.join("dev"), format!("13:{minor}\n")).unwrap();
            fs::write(node.join("device/name"), format!("{device_name}\n")).unwrap();
            fs::create_dir_all(sysfs.join("class/input")).unwrap();
            let leaf = devpath.rsplit('/').next().unwrap();
            symlink(&node, sysfs.join("class/input").join(leaf)).unwrap();
            fs::write(udev_data.join(format!("c13:{minor}")), db).unwrap();
        };
        add(
            &format!("{PREFIX}/3-1:1.0/0003:056A:0317.0001/input/input9/event9"),
            73,
            database,
            "Wacom Intuos5 touch L Pen",
        );
        add(
            "/devices/pci0000:00/0000:00:14.0/usb1/1-1/1-1:1.0/input/input2/event2",
            66,
            "E:ID_INPUT=1\nE:ID_INPUT_KEYBOARD=1\nG:seat\nG:uaccess\n",
            "Console keyboard",
        );
        Roots {
            sysfs,
            udev_data,
            dev,
        }
    }

    const ON_SEAT: &str = "E:ID_INPUT=1\nE:ID_INPUT_TABLET=1\nE:ID_INPUT.tags=arcen_bridge,arcen_bridge_58\nE:LIBINPUT_IGNORE_DEVICE=1\n";

    #[test]
    fn discovery_finds_only_the_ports_nodes_and_proves_their_seat() {
        let roots = tree("discover", ON_SEAT);
        let nodes =
            discover_nodes_in(&roots, PREFIX, &seat(), Duration::from_secs(3), &|| false).unwrap();
        assert_eq!(
            nodes.len(),
            1,
            "the console keyboard is never a bridged node"
        );
        assert_eq!(nodes[0].devnode, "/dev/input/event9");
        assert_eq!(nodes[0].role, BridgedNodeRole::Pen);
        assert_eq!(nodes[0].name, "Wacom Intuos5 touch L Pen");
        assert_eq!(verify_node_in(&roots, &nodes[0], PREFIX, &seat()), Ok(()));
        // Another desktop's bridge never verifies.
        let other = x11(59, 7);
        assert!(verify_node_in(&roots, &nodes[0], PREFIX, &other).is_err());
    }

    #[test]
    fn an_untagged_node_or_one_with_uaccess_fails_closed() {
        for database in [
            "E:ID_INPUT=1\nE:ID_INPUT_TABLET=1\nG:seat\nG:uaccess\n",
            "E:ID_INPUT=1\nE:ID_INPUT_TABLET=1\nG:seat\n",
            &format!("{ON_SEAT}G:uaccess\n"),
            &ON_SEAT.replace("arcen_bridge_58", "arcen_bridge_59"),
            &format!("{ON_SEAT}G:seat\n"),
        ] {
            let roots = tree("seat0", database);
            let error =
                discover_nodes_in(&roots, PREFIX, &seat(), Duration::from_millis(900), &|| {
                    false
                })
                .unwrap_err();
            assert!(error.contains("does not carry the bridge rule"), "{error}");
        }
    }

    /// Requirement (e): the login screen's X server (started after the
    /// ignore class, no -configdir) and another dedicated desktop (its own
    /// tag only, foreign input ignored) keep the bridge out; a server started
    /// before the class was installed does not.
    #[test]
    fn x_server_facts_come_from_arguments_config_and_start_time() {
        let dir = scratch("xservers");
        let config = dir.join("xorg.conf");
        fs::write(
            &config,
            format!(
                "Section \"InputClass\"\n    Identifier     \"{}\"\n    Option         \"Ignore\" \"on\"\nEndSection\n\n{}",
                arcen_input::virtual_device::FOREIGN_INPUT_CLASS,
                bridge_seat::xorg_session_bridge_class(58)
            ),
        )
        .unwrap();
        let installed = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
        let later = Some(installed + Duration::from_secs(5));
        let earlier = Some(installed - Duration::from_secs(5));
        let args = |list: &[&str]| {
            list.iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        };
        let config_path = config.to_string_lossy().into_owned();
        let desktop = x_server_facts(
            10,
            &args(&["/usr/libexec/Xorg", ":58", "-config", &config_path, "vt1"]),
            earlier,
            installed,
        );
        assert_eq!(desktop.display, Some(58));
        assert_eq!(desktop.admits_bridge, Some(58));
        assert!(desktop.ignores_foreign_input);
        // A desktop config with something after its bridge classes is not
        // taken as admitting (or confining) anything.
        let followed = dir.join("followed.conf");
        fs::write(
            &followed,
            format!(
                "{}Section \"InputClass\"\nEndSection\n",
                fs::read_to_string(&config).unwrap()
            ),
        )
        .unwrap();
        let followed_path = followed.to_string_lossy().into_owned();
        let followed = x_server_facts(
            11,
            &args(&["/usr/libexec/Xorg", ":59", "-config", &followed_path]),
            later,
            installed,
        );
        assert_eq!(followed.admits_bridge, None);
        assert!(
            bridge_seat::x11_isolation_in_force(true, &[desktop.clone(), followed], 58).is_err()
        );
        let greeter = x_server_facts(
            20,
            &args(&["/usr/libexec/Xorg", "vt1", "-displayfd", "3", "-keeptty"]),
            later,
            installed,
        );
        assert_eq!(greeter.display, None);
        assert!(greeter.loaded_bridge_ignore);
        assert_eq!(
            bridge_seat::x11_isolation_in_force(true, &[desktop.clone(), greeter], 58),
            Ok(())
        );
        let stale = x_server_facts(30, &args(&["/usr/libexec/Xorg", "vt1"]), earlier, installed);
        assert!(!stale.loaded_bridge_ignore);
        let configdir = x_server_facts(
            40,
            &args(&["/usr/libexec/Xorg", "-configdir", "/x"]),
            later,
            installed,
        );
        assert!(!configdir.loaded_bridge_ignore);
        for foreign in [stale, configdir] {
            assert!(
                bridge_seat::x11_isolation_in_force(true, &[desktop.clone(), foreign], 58).is_err()
            );
        }
    }

    #[test]
    fn a_process_start_time_is_read_after_its_name() {
        let boot = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
        // A name with spaces and parentheses cannot shift the fields.
        let stat =
            "3463 (Xorg (x) y) S 3461 3461 3461 1025 3461 4194560 1 0 0 0 0 0 0 0 20 0 2 0 500 0 0";
        // SAFETY: as in `process_start`.
        let per_second =
            u64::try_from(unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) }).unwrap();
        assert_eq!(
            process_start(stat, boot),
            Some(boot + Duration::from_secs(500 / per_second))
        );
        assert_eq!(process_start("garbage", boot), None);
    }

    #[test]
    fn discovery_stops_when_cancelled_and_device_numbers_parse() {
        let roots = tree("cancel", ON_SEAT);
        let started = Instant::now();
        let error = discover_nodes_in(&roots, PREFIX, &seat(), Duration::from_secs(30), &|| true)
            .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(parse_device_number("13:73\n"), Some((13, 73)));
        assert_eq!(parse_device_number("13"), None);
        assert_eq!(parse_device_number("a:1"), None);
    }

    #[test]
    fn a_forged_node_fact_is_refused() {
        let roots = tree("forged", ON_SEAT);
        let console = BridgedInputNode {
            devnode: "/dev/input/event2".into(),
            devpath: format!("{PREFIX}/3-1:1.0/input/input2/event2"),
            role: BridgedNodeRole::Pen,
            name: String::new(),
            kernel_driver: String::new(),
        };
        assert!(verify_node_in(&roots, &console, PREFIX, &seat()).is_err());
    }

    #[test]
    fn a_port_prefix_comes_only_from_a_virtual_controller_bus() {
        let root = scratch("bus");
        let sysfs = root.join("sys");
        for (bus, device) in [
            (3, "devices/platform/vhci_hcd.0/usb3"),
            (1, "devices/pci0000:00/0000:00:14.0/usb1"),
        ] {
            fs::create_dir_all(sysfs.join(device)).unwrap();
            fs::create_dir_all(sysfs.join("bus/usb/devices")).unwrap();
            symlink(
                sysfs.join(device),
                sysfs.join(format!("bus/usb/devices/usb{bus}")),
            )
            .unwrap();
        }
        assert_eq!(port_prefix_in(&sysfs, 3, 1).unwrap(), PREFIX);
        assert!(port_prefix_in(&sysfs, 1, 1).is_err());
        assert!(port_prefix_in(&sysfs, 9, 1).is_err());
    }
}
