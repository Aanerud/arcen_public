//! The session pen on a private udev seat: the lab seat pen
//! (`wayland.lab_seat_pen`) and the native seat (`wayland.session_mode`).
//!
//! Every headless Wayland desktop gets its own virtual tablet
//! ([`SeatPen::create`]) when it is created: a unique name, a volatile udev
//! rule that puts exactly that device on the desktop's
//! own seat (never seat0), and an ACL on its event node for the desktop's
//! user alone. A compositor built with the headless-input lab patch and
//! given `ARCEN_LAB_HEADLESS_INPUT_SEAT` runs libinput on that seat, so the
//! tablet is a real tablet in that session. Pen samples from the Deck
//! attached to that desktop are written to its tablet instead of the
//! emulated-input (EIS) seat, which has no stylus.
//!
//! With `wayland.session_mode` native the seat is a whole native seat instead
//! ([`SeatPen::create_native`]): a stock compositor runs natively on it and
//! logind brokers (and revokes) every device, so no ACL is granted.
//!
//! The desktop owns its pen: when the desktop ends, the grant is revoked,
//! the device destroyed and the rule removed ([`Drop`]). A Pier start sweeps
//! rules a crashed Pier left behind ([`sweep_stale_rules`]); its devices and
//! their ACLs died with its process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use arcen_input::seat_pen::{
    is_desktop_seat, is_mutter_rules_override, is_rule_file, mutter_rules_without_vkms_ignore,
    DesktopPen, NativeSeat, MUTTER_RULES_FILE, PRODUCT, RASTER, RESOLUTION_PER_MM, UDEV_RULE_DIR,
    VENDOR,
};
use arcen_input::virtual_device::TILT_UNITS_PER_RADIAN;
use arcen_protocol::messages::PenEventMsg;
use arcen_session::native_seat::{
    count_connectors, NativeSeatHostFacts, NativeSeatUnavailable, VkmsState,
};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    UinputAbsSetup,
};

use super::pen::{self, PenToolState};
use super::InputError;
use crate::logging::target;

/// Emits [`pen::proximity_prelude`] at (`x`, `y`): one genuine pen
/// proximity-in and -out, so libinput stops forcing the tool out of proximity
/// after 50 ms without events
/// (`arcen_input::virtual_device::ProximityTimeoutDisarm`).
pub(crate) fn emit_proximity_prelude(
    device: &mut VirtualDevice,
    x: i32,
    y: i32,
) -> std::io::Result<()> {
    for frame in pen::proximity_prelude(x, y) {
        let events: Vec<InputEvent> = frame
            .into_iter()
            .map(|(kind, code, value)| InputEvent::new(kind, code, value))
            .collect();
        device.emit(&events)?;
    }
    Ok(())
}

/// One desktop's tablet; dropping it takes the tablet away again.
pub struct SeatPen {
    pen: DesktopPen,
    uid: u32,
    node: Option<PathBuf>,
    rule_installed: bool,
    device: Mutex<Option<PenDevice>>,
    /// The KMS side of a native seat (`wayland.session_mode`), until it is
    /// taken apart ([`SeatPen::release_native_now`]).
    native: Mutex<Option<NativeKms>>,
    /// Whether this is a native seat (stays true after the release).
    native_mode: bool,
}

/// What a native seat changed on the host, so [`Drop`] undoes exactly that.
#[derive(Debug, Default)]
struct NativeKms {
    seat: Option<NativeSeat>,
    /// The compositor GPU's card (`/sys/class/drm/cardN`), retriggered back
    /// to `seat0` at the end.
    gpu_card: Option<PathBuf>,
    mutter_override: bool,
    vkms_loaded: bool,
    rule_installed: bool,
}

/// Exists while `vkms` is loaded by Arcen; a Pier start or exit that finds
/// it unloads `vkms` (and only then removes it).
const VKMS_MARKER: &str = "/run/arcen/vkms-loaded-by-arcen";
/// The rule a sweep uses to take any `vkms` card off its seat.
const VKMS_RELEASE_RULE: &str = "72-arcen-vkms-release.rules";

/// Why a desktop cannot have its native seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSeatError {
    pub reason: NativeSeatUnavailable,
    pub detail: String,
}

impl NativeSeatError {
    fn setup(detail: &str) -> Self {
        Self {
            reason: NativeSeatUnavailable::SeatSetupFailed,
            detail: detail.to_owned(),
        }
    }
}

impl std::fmt::Display for NativeSeatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.reason.token(), self.detail)
    }
}

const GREETER_ON_SEAT: &str = "a greeter is on the seat";

/// Where the distribution keeps Mutter's udev rules.
const DISTRIBUTION_MUTTER_RULES: &[&str] = &[
    "/usr/lib/udev/rules.d/61-mutter.rules",
    "/lib/udev/rules.d/61-mutter.rules",
];
/// How long a display manager may take to fail a greeter on the new seat
/// before the `vkms` head joins it (see [`SeatPen::create_native`]).
const GREETER_SETTLE: std::time::Duration = std::time::Duration::from_secs(4);
const SEAT_POLL: std::time::Duration = std::time::Duration::from_millis(100);
const SEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct PenDevice {
    device: VirtualDevice,
    state: PenToolState,
    proximity_timeout: arcen_input::virtual_device::ProximityTimeoutDisarm,
}

impl std::fmt::Debug for SeatPen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SeatPen")
            .field("seat", &self.pen.seat())
            .field("uid", &self.uid)
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

impl SeatPen {
    /// Creates a desktop's tablet for `uid`: installs its rule, creates the
    /// device, proves udev put it on the desktop's seat, and grants `uid`
    /// (alone) its event node. Must run before the desktop's compositor
    /// starts, which opens its seat's devices once.
    ///
    /// # Errors
    ///
    /// When any step fails; whatever was set up is undone, and the desktop
    /// gets no lab pen.
    pub fn create(uid: u32) -> Result<Arc<Self>, String> {
        if uid == 0 {
            return Err("the seat pen is never granted to root".into());
        }
        let mut id = [0_u8; 8];
        getrandom::getrandom(&mut id).map_err(|error| format!("desktop pen id: {error}"))?;
        let mut pen = Self {
            pen: DesktopPen::from_random(id),
            uid,
            node: None,
            rule_installed: false,
            device: Mutex::new(None),
            native: Mutex::new(None),
            native_mode: false,
        };
        pen.install_rule()?;
        let mut device = build(&pen.pen).map_err(|error| format!("seat pen: {error}"))?;
        run("udevadm", &["settle", "--timeout=5"])?;
        let node = device
            .enumerate_dev_nodes_blocking()
            .ok()
            .and_then(|mut nodes| nodes.find_map(Result::ok))
            .ok_or("the seat pen has no event node")?;
        *pen.device_lock() = Some(PenDevice {
            device,
            state: PenToolState::default(),
            proximity_timeout: arcen_input::virtual_device::ProximityTimeoutDisarm::default(),
        });
        pen.node = Some(node.clone());
        let seat = udev_seat(&node)?;
        if seat != pen.pen.seat() {
            return Err(format!(
                "udev put the seat pen on {seat:?}, not {}",
                pen.pen.seat()
            ));
        }
        let node_arg = node.to_string_lossy();
        for args in DesktopPen::grant_plan(&node_arg, uid) {
            run_owned("setfacl", &args)?;
        }
        tracing::warn!(
            target: target::INPUT,
            node = %node.display(),
            seat = %pen.pen.seat(),
            uid,
            "LAB: this desktop's pen was created on its own udev seat (wayland.lab_seat_pen)"
        );
        Ok(Arc::new(pen))
    }

    /// A desktop seat for a stock compositor running natively (not
    /// headless; `wayland.session_mode`, see [`arcen_session::native_seat`]).
    /// The seat gets the compositor GPU's card node (render), the in-tree
    /// `vkms` card (stock Mutter refuses to run natively without a
    /// connector, and a vGPU has none) and the desktop's tablet. logind
    /// hands the session's compositor every device through `TakeDevice` and
    /// revokes the input fds when the session ends (`EVIOCREVOKE`), so no
    /// ACL is granted.
    ///
    /// The order defends the seat against a display manager: GDM starts a
    /// greeter on every new graphical seat. The seat first exists with the
    /// GPU card only, where a greeter fails at once ("No GPUs with outputs
    /// found"); the `vkms` head joins after [`GREETER_SETTLE`], which changes
    /// no `CanGraphical` and so starts no greeter. A seat that still holds
    /// any session before the desktop's own opens is refused.
    ///
    /// One per host: `vkms` must not be loaded already, and the GPU card
    /// must not be open (an Xorg desktop on `seat0` would lose it).
    ///
    /// # Errors
    ///
    /// Why the seat cannot be had; whatever was set up is undone.
    pub fn create_native(uid: u32) -> Result<Arc<Self>, NativeSeatError> {
        if uid == 0 {
            return Err(NativeSeatError::setup(
                "the native seat is never given to root",
            ));
        }
        let facts = native_seat_host_facts();
        if let Some(reason) = facts.blocker() {
            return Err(NativeSeatError {
                reason,
                detail: reason.explanation().to_owned(),
            });
        }
        let (gpu_card, gpu_id_path) =
            compositor_gpu_card().map_err(|error| NativeSeatError::setup(&error))?;
        let native_seat = NativeSeat::new(&gpu_id_path)
            .ok_or_else(|| NativeSeatError::setup(&format!("GPU path {gpu_id_path:?}")))?;
        let mut id = [0_u8; 8];
        getrandom::getrandom(&mut id)
            .map_err(|error| NativeSeatError::setup(&format!("desktop pen id: {error}")))?;
        let mut pen = Self {
            pen: DesktopPen::from_random(id),
            uid,
            node: None,
            rule_installed: false,
            device: Mutex::new(None),
            native: Mutex::new(Some(NativeKms::default())),
            native_mode: true,
        };
        // Any failure from here drops `pen`, which undoes what was done.
        let vkms = pen
            .build_native_seat(&native_seat, &gpu_card)
            .map_err(|error| {
                if error.starts_with(GREETER_ON_SEAT) {
                    NativeSeatError {
                        reason: NativeSeatUnavailable::GreeterOnSeat,
                        detail: error,
                    }
                } else {
                    NativeSeatError::setup(&error)
                }
            })?;
        tracing::info!(
            target: target::INPUT,
            seat = %pen.pen.seat(),
            gpu = %gpu_id_path,
            vkms = %vkms.display(),
            pen = %pen.node.as_deref().unwrap_or(Path::new("")).display(),
            display_manager = facts.display_manager_active,
            uid,
            "this desktop runs its compositor natively on its own seat"
        );
        Ok(Arc::new(pen))
    }

    fn build_native_seat(
        &mut self,
        native_seat: &NativeSeat,
        gpu_card: &Path,
    ) -> Result<PathBuf, String> {
        let seat = self.pen.seat();
        let rule = self.pen.native_udev_rule(native_seat);
        self.install_rule_text(&rule)?;
        self.rule_installed = false;
        if let Some(native) = self.kms_mut() {
            native.rule_installed = true;
            native.seat = Some(native_seat.clone());
        }
        self.install_mutter_override()?;
        run("udevadm", &["control", "--reload"])?;
        let card_arg = gpu_card.to_string_lossy().into_owned();
        if let Some(native) = self.kms_mut() {
            native.gpu_card = Some(gpu_card.to_path_buf());
        }
        run(
            "udevadm",
            &["trigger", "--action=change", "--settle", &card_arg],
        )?;
        wait_for(|| seat_exists(&seat)).ok_or_else(|| format!("logind never created {seat}"))?;
        std::thread::sleep(GREETER_SETTLE);
        run("modprobe", &["vkms"])?;
        if let Some(native) = self.kms_mut() {
            native.vkms_loaded = true;
        }
        if let Err(error) = std::fs::create_dir_all("/run/arcen")
            .and_then(|()| std::fs::write(VKMS_MARKER, b"vkms loaded by an Arcen native seat\n"))
        {
            tracing::warn!(target: target::INPUT, %error, "the vkms marker was not written; a crashed Pier's vkms is then found by its seat");
        }
        run("udevadm", &["settle", "--timeout=5"])?;
        let vkms = vkms_card().ok_or("vkms created no card")?;
        for card in [gpu_card, vkms.as_path()] {
            let node = PathBuf::from("/dev/dri").join(card.file_name().unwrap_or_default());
            let on = udev_seat(&node)?;
            if on != seat {
                return Err(format!("udev put {} on {on:?}, not {seat}", node.display()));
            }
        }
        let mut device = build(&self.pen).map_err(|error| format!("seat pen: {error}"))?;
        run("udevadm", &["settle", "--timeout=5"])?;
        let node = device
            .enumerate_dev_nodes_blocking()
            .ok()
            .and_then(|mut nodes| nodes.find_map(Result::ok))
            .ok_or("the seat pen has no event node")?;
        *self.device_lock() = Some(PenDevice {
            device,
            state: PenToolState::default(),
            proximity_timeout: arcen_input::virtual_device::ProximityTimeoutDisarm::default(),
        });
        self.node = Some(node.clone());
        let on = udev_seat(&node)?;
        if on != seat {
            return Err(format!("udev put the seat pen on {on:?}, not {seat}"));
        }
        let sessions = seat_sessions(&seat)?;
        if !sessions.is_empty() {
            return Err(format!(
                "{GREETER_ON_SEAT}: {seat} already holds session(s) {sessions}"
            ));
        }
        Ok(vkms)
    }

    /// The tablet's device name, as libinput and `zwp_tablet_v2` report it.
    #[must_use]
    pub fn device_name(&self) -> String {
        self.pen.device_name()
    }

    /// Brings the pen into proximity at the raster centre with a little
    /// tilt and takes it out again: the compositor then announces the tool
    /// to its tablet clients (the qualification proof).
    ///
    /// # Errors
    ///
    /// When the device is gone or the kernel refuses the events.
    pub fn bring_into_proximity(&self) -> Result<(), InputError> {
        let mut guard = self.device_lock();
        let pen = guard.as_mut().ok_or(InputError::PenUnavailable)?;
        let centre = i32::try_from(RASTER / 2).unwrap_or(0);
        let key = |code, value| InputEvent::new(EventType::KEY.0, code, value);
        let abs =
            |axis: AbsoluteAxisCode, value| InputEvent::new(EventType::ABSOLUTE.0, axis.0, value);
        pen.device.emit(&[
            abs(AbsoluteAxisCode::ABS_X, centre),
            abs(AbsoluteAxisCode::ABS_Y, centre),
            abs(AbsoluteAxisCode::ABS_PRESSURE, 0),
            abs(AbsoluteAxisCode::ABS_TILT_X, 10),
            abs(AbsoluteAxisCode::ABS_TILT_Y, -10),
            key(pen::BTN_TOOL_PEN, 1),
        ])?;
        std::thread::sleep(std::time::Duration::from_millis(30));
        pen.device.emit(&[
            abs(AbsoluteAxisCode::ABS_X, centre + 100),
            abs(AbsoluteAxisCode::ABS_TILT_X, 12),
        ])?;
        std::thread::sleep(std::time::Duration::from_millis(30));
        pen.device.emit(&[key(pen::BTN_TOOL_PEN, 0)])?;
        pen.state = PenToolState::default();
        Ok(())
    }

    /// Whether this desktop's compositor runs natively on the pen's seat.
    #[must_use]
    pub fn is_native(&self) -> bool {
        self.native_mode
    }

    fn kms_mut(&mut self) -> Option<&mut NativeKms> {
        self.native
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
    }

    /// The udev seat the desktop's compositor runs libinput on.
    #[must_use]
    pub fn seat(&self) -> String {
        self.pen.seat()
    }

    /// The desktop user this pen is granted to.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Writes one validated Deck pen sample.
    ///
    /// # Errors
    ///
    /// When the device is gone or the kernel refuses the events.
    pub fn pen_event(&self, message: &PenEventMsg) -> Result<(), InputError> {
        let mut guard = self.device_lock();
        let pen = guard.as_mut().ok_or(InputError::PenUnavailable)?;
        let axes = pen::PenAxes::from_event(message, RASTER, RASTER, pen::SEAT_PEN_CONTACT_FLOOR);
        if pen
            .proximity_timeout
            .needs_prelude(pen.state.tool.is_some(), message.in_proximity)
        {
            emit_proximity_prelude(&mut pen.device, axes.x, axes.y)?;
        }
        let (edges, new_state) = pen::plan_pen_edges(pen.state, message);
        let mut events: Vec<InputEvent> = edges
            .iter()
            .map(|&(code, pressed)| InputEvent::new(EventType::KEY.0, code, i32::from(pressed)))
            .collect();
        if message.in_proximity {
            for (axis, value) in [
                (AbsoluteAxisCode::ABS_X, axes.x),
                (AbsoluteAxisCode::ABS_Y, axes.y),
                (AbsoluteAxisCode::ABS_PRESSURE, axes.pressure),
                (AbsoluteAxisCode::ABS_TILT_X, axes.tilt_x),
                (AbsoluteAxisCode::ABS_TILT_Y, axes.tilt_y),
            ] {
                events.push(InputEvent::new(EventType::ABSOLUTE.0, axis.0, value));
            }
        }
        pen.device.emit(&events)?;
        pen.state = new_state;
        Ok(())
    }

    /// Lifts the pen out of proximity (tip, buttons, then the tool), as when
    /// an attachment ends with the pen down.
    pub fn release(&self) {
        let mut guard = self.device_lock();
        let Some(pen) = guard.as_mut() else {
            return;
        };
        if pen.state.tool.is_none() {
            return;
        }
        let mut events = Vec::new();
        for (held, code) in [
            (pen.state.touching, pen::BTN_TOUCH),
            (pen.state.stylus, pen::BTN_STYLUS),
            (pen.state.stylus2, pen::BTN_STYLUS2),
        ] {
            if held {
                events.push(InputEvent::new(EventType::KEY.0, code, 0));
            }
        }
        events.push(InputEvent::new(EventType::KEY.0, pen::BTN_TOOL_PEN, 0));
        events.push(InputEvent::new(EventType::KEY.0, pen::BTN_TOOL_RUBBER, 0));
        let _ = pen.device.emit(&events);
        pen.state = PenToolState::default();
    }

    fn install_rule(&mut self) -> Result<(), String> {
        let rule = self.pen.udev_rule();
        self.install_rule_text(&rule)?;
        run("udevadm", &["control", "--reload"])
    }

    fn install_rule_text(&mut self, rule: &str) -> Result<(), String> {
        let path = self.pen.rule_path();
        std::fs::create_dir_all(UDEV_RULE_DIR)
            .map_err(|error| format!("{UDEV_RULE_DIR}: {error}"))?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, rule.as_bytes()))
            .map_err(|error| format!("{path}: {error}"))?;
        self.rule_installed = true;
        Ok(())
    }

    /// Installs the volatile `61-mutter.rules` override (no `vkms` ignore
    /// tag) unless one is already there; a file of someone else's is never
    /// replaced.
    fn install_mutter_override(&mut self) -> Result<(), String> {
        let path = Path::new(UDEV_RULE_DIR).join(MUTTER_RULES_FILE);
        if let Ok(existing) = std::fs::read_to_string(&path) {
            return if is_mutter_rules_override(&existing) {
                Ok(())
            } else {
                Err(format!("{} exists and is not Arcen's", path.display()))
            };
        }
        // usr-merged and split-/usr layouts; a distribution without the file
        // has no vkms ignore tag to undo.
        let Some(distribution) = DISTRIBUTION_MUTTER_RULES
            .iter()
            .find_map(|candidate| std::fs::read_to_string(candidate).ok())
        else {
            return Ok(());
        };
        if !distribution
            .lines()
            .any(|line| line.contains("vkms") && line.contains("mutter-device-ignore"))
        {
            return Ok(());
        }
        std::fs::write(&path, mutter_rules_without_vkms_ignore(&distribution))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if let Some(native) = self.kms_mut() {
            native.mutter_override = true;
        }
        Ok(())
    }

    /// Takes the native seat apart, once: the pen is lifted and destroyed,
    /// `vkms` leaves the seat by tag (the seat and any greeter on it go with
    /// it) and is unloaded, the rules are removed and the GPU card returns to
    /// `seat0`. Blocking (it waits for the compositor to let go of `vkms`):
    /// call it on a blocking thread after the desktop's session has ended.
    pub fn release_native_now(&self) {
        let Some(native) = self
            .native
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        self.release();
        drop(self.device_lock().take());
        let seat = self.pen.seat();
        let mut unloaded = true;
        if native.vkms_loaded {
            let path = self.pen.rule_path();
            let _ = std::fs::write(&path, self.pen.native_release_rule());
            let _ = run("udevadm", &["control", "--reload"]);
            if let Some(card) = vkms_card() {
                let card = card.to_string_lossy().into_owned();
                let _ = run(
                    "udevadm",
                    &["trigger", "--action=change", "--settle", &card],
                );
            }
            let _ = wait_for(|| seat_exists(&seat).is_none().then_some(()));
            unloaded = unload_vkms();
            if !unloaded {
                tracing::warn!(target: target::INPUT, %seat, "vkms is still in use and stays loaded; the next Pier start or exit unloads it");
            }
        }
        if native.mutter_override {
            let _ = std::fs::remove_file(Path::new(UDEV_RULE_DIR).join(MUTTER_RULES_FILE));
        }
        // A vkms still loaded keeps its release rule, so a later change
        // event never puts it on seat0 (the start/exit sweep finishes).
        if native.rule_installed && unloaded {
            let _ = std::fs::remove_file(self.pen.rule_path());
        }
        let _ = run("udevadm", &["control", "--reload"]);
        if let Some(card) = native.gpu_card {
            let card = card.to_string_lossy().into_owned();
            let _ = run(
                "udevadm",
                &["trigger", "--action=change", "--settle", &card],
            );
        }
        tracing::info!(target: target::INPUT, %seat, vkms_unloaded = unloaded, "the native seat was taken apart (vkms released, rules removed, GPU card back on seat0)");
    }

    fn device_lock(&self) -> std::sync::MutexGuard<'_, Option<PenDevice>> {
        self.device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for SeatPen {
    /// Revokes the grant, destroys the device, then removes the rule.
    fn drop(&mut self) {
        if self.native_mode {
            // Normally done already, on a blocking thread, by the
            // launcher's shutdown; this is the last resort.
            self.release_native_now();
            return;
        }
        self.release();
        if let Some(node) = self.node.as_ref() {
            let node = node.to_string_lossy();
            if let Err(error) = run_owned("setfacl", &DesktopPen::revoke_plan(&node)) {
                tracing::warn!(target: target::INPUT, %error, "LAB: the seat pen grant was not revoked; the device is destroyed anyway");
            }
        }
        drop(self.device_lock().take());
        if self.rule_installed {
            let path = self.pen.rule_path();
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::warn!(target: target::INPUT, %error, path, "LAB: the seat pen rule was not removed");
            }
            let _ = run("udevadm", &["control", "--reload"]);
        }
        tracing::info!(
            target: target::INPUT,
            seat = %self.pen.seat(),
            uid = self.uid,
            "LAB: the desktop's pen was revoked, destroyed and its rule removed"
        );
    }
}

/// Removes every seat-pen rule in `dir` (a Pier that crashed left them);
/// returns how many. Its devices, and their ACLs, died with that Pier.
pub fn sweep_stale_rules_in(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_str().is_some_and(is_rule_file))
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// Sweeps stale seat-pen rules from [`UDEV_RULE_DIR`] at Pier start and
/// exit, whatever `wayland.lab_seat_pen` says, and reloads udev when there
/// were any. A `vkms` that an Arcen native seat loaded (its marker exists,
/// or its card is still on an Arcen desktop seat) is taken off any seat and
/// unloaded, whether or not a rule was left behind.
pub fn sweep_stale_rules() {
    let mut removed = sweep_stale_rules_in(Path::new(UDEV_RULE_DIR));
    let override_path = Path::new(UDEV_RULE_DIR).join(MUTTER_RULES_FILE);
    if std::fs::read_to_string(&override_path).is_ok_and(|text| is_mutter_rules_override(&text))
        && std::fs::remove_file(&override_path).is_ok()
    {
        removed += 1;
    }
    let vkms_ours = Path::new(VKMS_MARKER).exists()
        || vkms_card()
            .and_then(|card| udev_seat(&PathBuf::from("/dev/dri").join(card.file_name()?)).ok())
            .is_some_and(|seat| is_desktop_seat(&seat));
    if removed > 0 {
        let _ = run("udevadm", &["control", "--reload"]);
    }
    if vkms_ours && Path::new("/sys/module/vkms").exists() {
        let rule = Path::new(UDEV_RULE_DIR).join(VKMS_RELEASE_RULE);
        let _ = std::fs::create_dir_all(UDEV_RULE_DIR);
        let _ = std::fs::write(
            &rule,
            "# Arcen: a stale vkms leaves every seat before it is unloaded.\n\
             ACTION!=\"remove\", SUBSYSTEM==\"drm\", KERNEL==\"card[0-9]*\", DEVPATH==\"*/vkms/*\", TAG-=\"master-of-seat\", TAG-=\"seat\"\n",
        );
        let _ = run("udevadm", &["control", "--reload"]);
        if let Some(card) = vkms_card() {
            let card = card.to_string_lossy().into_owned();
            let _ = run(
                "udevadm",
                &["trigger", "--action=change", "--settle", &card],
            );
        }
        let unloaded = unload_vkms();
        // Kept while vkms is still loaded, so it never lands on seat0.
        if unloaded {
            let _ = std::fs::remove_file(&rule);
            let _ = run("udevadm", &["control", "--reload"]);
        }
        tracing::warn!(
            target: target::INPUT,
            unloaded,
            "a vkms an Arcen native seat loaded was still there; it was taken off its seat{}",
            if unloaded { " and unloaded" } else { ", but is still in use" }
        );
    } else if !Path::new("/sys/module/vkms").exists() {
        let _ = std::fs::remove_file(VKMS_MARKER);
    }
    if removed > 0 || vkms_ours {
        // A native seat's GPU card returns to seat0.
        if let Ok((card, _)) = compositor_gpu_card() {
            let card = card.to_string_lossy().into_owned();
            let _ = run(
                "udevadm",
                &["trigger", "--action=change", "--settle", &card],
            );
        }
    }
    if removed > 0 {
        tracing::warn!(
            target: target::INPUT,
            removed,
            "removed seat pen udev rules a previous Pier left behind"
        );
    }
}

/// Unloads `vkms`, retrying while a compositor is still letting go of it
/// (up to 15 s); removes the marker once it is gone.
fn unload_vkms() -> bool {
    let unloaded = (0..30).any(|_| {
        if !Path::new("/sys/module/vkms").exists() || run("modprobe", &["-r", "vkms"]).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        false
    });
    if unloaded {
        let _ = std::fs::remove_file(VKMS_MARKER);
    }
    unloaded
}

/// What this host has for a native desktop, read now.
#[must_use]
pub fn native_seat_host_facts() -> NativeSeatHostFacts {
    let card = compositor_gpu_card().ok();
    let names = std::fs::read_dir("/sys/class/drm")
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let card_name = card
        .as_ref()
        .and_then(|(path, _)| path.file_name())
        .map(|name| name.to_string_lossy().into_owned());
    NativeSeatHostFacts {
        gpu_connectors: card_name.as_deref().map_or(0, |name| {
            count_connectors(name, names.iter().map(String::as_str))
        }),
        gpu_open: card_name
            .as_deref()
            .is_some_and(|name| node_is_open(&Path::new("/dev/dri").join(name))),
        gpu_id_path: card.map(|(_, id_path)| id_path),
        polkit: crate::session::polkit_remote::state(),
        vkms: if Path::new("/sys/module/vkms").exists() {
            VkmsState::Loaded
        } else if Command::new("modprobe")
            .args(["--dry-run", "--quiet", "vkms"])
            .status()
            .is_ok_and(|status| status.success())
        {
            VkmsState::Available
        } else {
            VkmsState::Missing
        },
        display_manager_active: Command::new("systemctl")
            .args(["is-active", "--quiet", "display-manager.service"])
            .status()
            .is_ok_and(|status| status.success()),
    }
}

/// The compositor GPU's card (`/sys/class/drm/cardN`) and its udev
/// `ID_PATH` (`pci-…`): the first NVIDIA card, the only GPU capenc encodes on.
fn compositor_gpu_card() -> Result<(PathBuf, String), String> {
    let mut cards = std::fs::read_dir("/sys/class/drm")
        .map_err(|error| format!("/sys/class/drm: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("card"))
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect::<Vec<_>>();
    cards.sort();
    for card in cards {
        let vendor = std::fs::read_to_string(card.join("device/vendor")).unwrap_or_default();
        if vendor.trim() != "0x10de" {
            continue;
        }
        let pci = std::fs::canonicalize(card.join("device"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .ok_or("the NVIDIA card has no PCI device")?;
        return Ok((card, format!("pci-{pci}")));
    }
    Err("no NVIDIA DRM card (nvidia_drm modeset=1 is required)".into())
}

/// The `vkms` card's sysfs path, when the driver is loaded.
fn vkms_card() -> Option<PathBuf> {
    std::fs::read_dir("/sys/class/drm")
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("card"))
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        })
        .find(|path| {
            std::fs::canonicalize(path).is_ok_and(|real| real.to_string_lossy().contains("/vkms/"))
        })
}

/// Whether any process holds `node` open.
fn node_is_open(node: &Path) -> bool {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return false;
    };
    processes.filter_map(Result::ok).any(|process| {
        std::fs::read_dir(process.path().join("fd")).is_ok_and(|fds| {
            fds.filter_map(Result::ok)
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == node))
        })
    })
}

fn seat_exists(seat: &str) -> Option<()> {
    Command::new("loginctl")
        .args(["show-seat", seat, "-p", "Id", "--value"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| ())
}

/// The sessions logind has on `seat`, space-separated (empty: none).
fn seat_sessions(seat: &str) -> Result<String, String> {
    let output = Command::new("loginctl")
        .args(["show-seat", seat, "-p", "Sessions", "--value"])
        .output()
        .map_err(|error| format!("loginctl: {error}"))?;
    if !output.status.success() {
        return Err(format!("loginctl show-seat {seat}: {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn wait_for<T>(mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = std::time::Instant::now() + SEAT_TIMEOUT;
    loop {
        if let Some(value) = check() {
            return Some(value);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(SEAT_POLL);
    }
}

fn udev_seat(node: &Path) -> Result<String, String> {
    let output = Command::new("udevadm")
        .args(["info", "--query=property", "--name"])
        .arg(node)
        .output()
        .map_err(|error| format!("udevadm: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "udevadm info {}: {}",
            node.display(),
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("ID_SEAT="))
        .unwrap_or("seat0")
        .to_owned())
}

fn build(pen: &DesktopPen) -> Result<VirtualDevice, InputError> {
    let keys = AttributeSet::from_iter([
        KeyCode(pen::BTN_TOOL_PEN),
        KeyCode(pen::BTN_TOOL_RUBBER),
        KeyCode(pen::BTN_TOUCH),
        KeyCode(pen::BTN_STYLUS),
        KeyCode(pen::BTN_STYLUS2),
    ]);
    let max = i32::try_from(RASTER - 1).unwrap_or(i32::MAX);
    let axis = |code, min, max, resolution| {
        UinputAbsSetup::new(code, AbsInfo::new(0, min, max, 0, 0, resolution))
    };
    // No phys: evdev 0.13's `with_phys` encodes `UI_SET_PHYS` with the
    // wrong size and the kernel refuses it (EINVAL). The name is unique.
    let name = pen.device_name();
    let device = VirtualDevice::builder()?
        .name(&name)
        .input_id(InputId::new(BusType::BUS_VIRTUAL, VENDOR, PRODUCT, 1))
        .with_keys(&keys)?
        .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, 0, max, RESOLUTION_PER_MM))?
        .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, 0, max, RESOLUTION_PER_MM))?
        .with_absolute_axis(&axis(
            AbsoluteAxisCode::ABS_PRESSURE,
            0,
            pen::PRESSURE_MAX,
            0,
        ))?
        .with_absolute_axis(&axis(
            AbsoluteAxisCode::ABS_TILT_X,
            pen::TILT_MIN_DEGREES,
            pen::TILT_MAX_DEGREES,
            TILT_UNITS_PER_RADIAN,
        ))?
        .with_absolute_axis(&axis(
            AbsoluteAxisCode::ABS_TILT_Y,
            pen::TILT_MIN_DEGREES,
            pen::TILT_MAX_DEGREES,
            TILT_UNITS_PER_RADIAN,
        ))?
        .build()?;
    Ok(device)
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("{program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {}: {status}", args.join(" ")))
    }
}

fn run_owned(program: &str, args: &[String]) -> Result<(), String> {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    run(program, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polkit_classifies_the_seats_this_module_builds_as_arcen_seats() {
        assert_eq!(
            arcen_input::seat_pen::SEAT_PREFIX,
            arcen_session::polkit_remote::ARCEN_SEAT_PREFIX
        );
    }

    #[test]
    fn the_sweep_removes_only_seat_pen_rules() {
        let dir = std::env::temp_dir().join(format!("arcen-seat-pen-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ours = [
            "72-arcen-lab-seat-pen.rules",
            "72-arcen-lab-seat-pen-0123456789abcdef.rules",
            "72-arcen-lab-seat-pen-fedcba9876543210.rules",
        ];
        let theirs = ["70-uaccess.rules", "72-arcen-lab-seat-pen-x.rules"];
        for name in ours.iter().chain(&theirs) {
            std::fs::write(dir.join(name), "").unwrap();
        }
        assert_eq!(sweep_stale_rules_in(&dir), ours.len());
        for name in ours {
            assert!(!dir.join(name).exists(), "{name}");
        }
        for name in theirs {
            assert!(dir.join(name).exists(), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(sweep_stale_rules_in(&dir), 0);
    }
}
