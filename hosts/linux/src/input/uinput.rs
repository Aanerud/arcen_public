use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use arcen_protocol::messages::{
    KeyEventMsg, MouseButtonMsg, MouseMoveMsg, MouseMoveRelativeMsg, MouseScrollMsg, PenEventMsg,
    PenToolMsg, PointerMotionMode, RegionPenEventMsg, RegionPointerButtonMsg,
    RegionPointerEnterMsg, RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg,
};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    RelativeAxisCode, UinputAbsSetup,
};

use super::keymap::{
    qt_key_to_evdev, LOCK_CODES, MODIFIER_CODES, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT,
};
use super::pen::{self, PenToolState};
use super::region_adapter::{RegionInputAdapter, XorgAxisPoint};
use super::{InputError, InputStats};

pub struct InputController {
    absolute_device: VirtualDevice,
    relative_device: VirtualDevice,
    // Separate virtual tablet-tool device: Xorg/libinput classify a device by
    // its full capability set, and merging tablet ABS_PRESSURE/BTN_TOOL_PEN
    // capabilities onto the existing absolute-mouse device would make that
    // device advertise (and get treated as) a tablet everywhere the plain
    // mouse pointer is expected. `None` when the probe/create attempt in
    // `new` failed; mouse/keyboard stay fully usable in that case.
    tablet_device: Option<VirtualDevice>,
    width: u32,
    height: u32,
    region_input: Option<RegionInputAdapter>,
    held_keys: HashSet<u16>,
    held_buttons: HashSet<u16>,
    lock_state: [bool; 3],
    pen_state: PenToolState,
    stats: Arc<InputStats>,
}

struct PenInjection {
    axes: pen::PenAxes,
    edges: Vec<(u16, bool)>,
    new_state: PenToolState,
    pressure: f32,
    tilt_x_degrees: f32,
    tilt_y_degrees: f32,
    tool: PenToolMsg,
    in_proximity: bool,
    touching: bool,
    sequence: u64,
    source: &'static str,
}

impl InputController {
    /// Creates the uinput device.
    ///
    /// `width × height` sets the fixed ABS axis range and must match the X11
    /// raster. ViewPortIn changes only capture scaling; pointer coordinates
    /// stay normalized across this fixed X screen.
    pub fn new(
        width: u32,
        height: u32,
        region_input: Option<RegionInputAdapter>,
    ) -> Result<(Self, Arc<InputStats>), InputError> {
        if width == 0 || height == 0 {
            return Err(InputError::InvalidGeometry(width, height));
        }
        if let Some(adapter) = region_input.as_ref() {
            let (region_width, region_height) = adapter.raster_size();
            if (region_width, region_height) != (width, height) {
                return Err(InputError::InvalidGeometry(region_width, region_height));
            }
        }
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=248u16 {
            keys.insert(KeyCode(code));
        }
        for code in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE] {
            keys.insert(code);
        }
        let scroll_axes =
            AttributeSet::from_iter([RelativeAxisCode::REL_WHEEL, RelativeAxisCode::REL_HWHEEL]);
        let x_axis = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, 0, width.saturating_sub(1) as i32, 0, 0, 1),
        );
        let y_axis = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, height.saturating_sub(1) as i32, 0, 0, 1),
        );
        // Xorg's libinput driver initializes a device advertising REL_X/Y as
        // relative and then discards ABS_X/Y events from it. Keep absolute and
        // relative motion on distinct kernel devices.
        let absolute_device = VirtualDevice::builder()?
            .name("Arcen Virtual Absolute Input")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0xA2CE, 0x0001, 1))
            .with_keys(&keys)?
            .with_absolute_axis(&x_axis)?
            .with_absolute_axis(&y_axis)?
            .with_relative_axes(&scroll_axes)?
            .build()?;
        let pointer_buttons =
            AttributeSet::from_iter([KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE]);
        let relative_axes =
            AttributeSet::from_iter([RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y]);
        let relative_device = VirtualDevice::builder()?
            .name("Arcen Virtual Relative Pointer")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0xA2CE, 0x0002, 1))
            .with_keys(&pointer_buttons)?
            .with_relative_axes(&relative_axes)?
            .build()?;
        // Probed/created here (before `ServerHello` is ever built) so the
        // host can advertise only the pen capability truth this process
        // actually established. A tablet-tool creation failure is logged and
        // downgraded to `tablet_device: None` rather than propagated: mouse
        // and keyboard must stay available even when the pen backend is not.
        let uinput_node_exists = std::path::Path::new("/dev/uinput").exists();
        let tablet_device = match build_tablet_device(width, height) {
            Ok(device) => {
                tracing::info!(
                    target: "input",
                    "pen/tablet uinput device created; pen input available for this session",
                );
                Some(device)
            }
            Err(error) => {
                // Log with enough detail to diagnose the common failure modes
                // (missing uinput kernel module, insufficient /dev/uinput
                // permissions, device-node creation limit reached). The host
                // will advertise pen = Unavailable in ServerHello so the client
                // knows typed pen is not available on this session.
                //
                // Extract the OS error kind from `error` itself rather than
                // calling `last_os_error()` — the errno register may already
                // have been overwritten by the time the tracing macro fires.
                let io_kind = if let InputError::Io(ref io_err) = error {
                    Some(io_err.kind())
                } else {
                    None
                };
                tracing::warn!(
                    target: "input",
                    error = %error,
                    error_kind = ?io_kind,
                    uinput_node_exists,
                    suggestion = if uinput_node_exists {
                        "check that the pier process has write permission on /dev/uinput \
                         (ls -la /dev/uinput) and the uinput kernel module is loaded \
                         (lsmod | grep uinput)"
                    } else {
                        "the /dev/uinput device node does not exist; load the kernel module \
                         with 'modprobe uinput' and verify with 'ls /dev/uinput'"
                    },
                    "pen/tablet uinput device unavailable; pen input disabled, \
                     mouse/keyboard remain active"
                );
                None
            }
        };
        let stats = Arc::new(InputStats::default());
        Ok((
            Self {
                absolute_device,
                relative_device,
                tablet_device,
                width,
                height,
                region_input,
                held_keys: HashSet::new(),
                held_buttons: HashSet::new(),
                lock_state: [false; 3],
                pen_state: PenToolState::default(),
                stats: stats.clone(),
            },
            stats,
        ))
    }

    pub fn key_event(&mut self, message: &KeyEventMsg) -> Result<(), InputError> {
        self.sync_lock_states(message)?;
        let Some(code) = qt_key_to_evdev(message.scan_code, message.modifiers) else {
            self.stats.unmapped_keys.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let edges = plan_key_edges(
            &mut self.held_keys,
            code,
            message.pressed,
            message.modifiers,
        );
        if !edges.is_empty() {
            let events: Vec<InputEvent> = edges
                .into_iter()
                .map(|(edge_code, pressed)| {
                    InputEvent::new(EventType::KEY.0, edge_code, i32::from(pressed))
                })
                .collect();
            self.absolute_device.emit(&events)?;
        }
        self.stats.key_events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Releases only keyboard keys/modifiers.
    ///
    /// `key_reset_modifiers` is a keyboard protocol edge. It must not clear
    /// mouse buttons, pen state, or region pointer focus while the pointer is
    /// crossing native Deck windows.
    pub fn reset_keyboard_held(&mut self) -> Result<(), InputError> {
        let codes = plan_keyboard_reset_releases(&self.held_keys);
        if !codes.is_empty() {
            let events: Vec<InputEvent> = codes
                .into_iter()
                .map(|code| InputEvent::new(EventType::KEY.0, code, 0))
                .collect();
            self.absolute_device.emit(&events)?;
        }
        self.held_keys.clear();
        self.stats.resets.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Releases all keyboard, pointer, and pen state during full teardown.
    pub fn reset_held(&mut self) -> Result<(), InputError> {
        let codes = plan_reset_releases(&self.held_keys, &self.held_buttons);
        if !codes.is_empty() {
            let events: Vec<InputEvent> = codes
                .into_iter()
                .map(|code| InputEvent::new(EventType::KEY.0, code, 0))
                .collect();
            self.absolute_device.emit(&events)?;
        }
        self.held_keys.clear();
        self.held_buttons.clear();
        if let Some(region_input) = self.region_input.as_mut() {
            let _ = region_input.release_all();
        }
        self.reset_pen_held()?;
        self.stats.resets.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Releases every held tablet-tool bit (proximity/touch/buttons) on the
    /// separate pen device, if one was created. Idempotent: a no-op when
    /// nothing is currently held or the pen backend is unavailable.
    fn reset_pen_held(&mut self) -> Result<(), InputError> {
        let codes = self.pen_state.held_codes();
        if let Some(tablet_device) = self.tablet_device.as_mut() {
            if !codes.is_empty() {
                let events: Vec<InputEvent> = codes
                    .into_iter()
                    .map(|code| InputEvent::new(EventType::KEY.0, code, 0))
                    .collect();
                tablet_device.emit(&events)?;
            }
        }
        self.pen_state = PenToolState::default();
        Ok(())
    }

    pub fn mouse_move(&mut self, message: &MouseMoveMsg) -> Result<(), InputError> {
        self.absolute_device
            .emit(&self.position_events(message.x, message.y))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn mouse_move_relative(
        &mut self,
        message: &MouseMoveRelativeMsg,
    ) -> Result<(), InputError> {
        self.relative_device
            .emit(&relative_motion_events(message.dx, message.dy))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn mouse_button(&mut self, message: &MouseButtonMsg) -> Result<(), InputError> {
        let code = mouse_button_code(message.button)?;
        let mut events = if prepends_absolute_position(message.motion_mode) {
            self.position_events(message.x, message.y).to_vec()
        } else {
            Vec::with_capacity(1)
        };
        let edge = plan_button_edge(&self.held_buttons, code, message.pressed);
        if let Some((edge_code, pressed)) = edge {
            events.push(InputEvent::new(
                EventType::KEY.0,
                edge_code,
                i32::from(pressed),
            ));
        }
        self.absolute_device.emit(&events)?;
        if let Some((edge_code, pressed)) = edge {
            commit_button_edge(&mut self.held_buttons, edge_code, pressed);
        }
        self.stats.mouse_buttons.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn mouse_scroll(&mut self, message: &MouseScrollMsg) -> Result<(), InputError> {
        let mut events = if prepends_absolute_position(message.motion_mode) {
            self.position_events(message.x, message.y).to_vec()
        } else {
            Vec::with_capacity(2)
        };
        let vertical = scroll_steps(message.dy);
        let horizontal = scroll_steps(message.dx);
        if vertical != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL.0,
                vertical,
            ));
        }
        if horizontal != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_HWHEEL.0,
                horizontal,
            ));
        }
        if !events.is_empty() {
            self.absolute_device.emit(&events)?;
        }
        self.stats.scroll_events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn region_pointer_enter(
        &mut self,
        message: &RegionPointerEnterMsg,
    ) -> Result<(), InputError> {
        let point = self
            .region_input
            .as_mut()
            .ok_or(InputError::RegionUnavailable)?
            .pointer_enter(message)?;
        self.absolute_device.emit(&axis_position_events(point))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn region_pointer_leave(
        &mut self,
        message: &RegionPointerLeaveMsg,
    ) -> Result<(), InputError> {
        let point = self
            .region_input
            .as_mut()
            .ok_or(InputError::RegionUnavailable)?
            .pointer_leave(message)?;
        self.absolute_device.emit(&axis_position_events(point))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn region_pointer_motion(
        &mut self,
        message: &RegionPointerMotionMsg,
    ) -> Result<(), InputError> {
        let point = self
            .region_input
            .as_mut()
            .ok_or(InputError::RegionUnavailable)?
            .pointer_motion(message)?;
        self.absolute_device.emit(&axis_position_events(point))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn region_pointer_button(
        &mut self,
        message: &RegionPointerButtonMsg,
    ) -> Result<(), InputError> {
        let code = mouse_button_code(message.button)?;
        let mapped = self
            .region_input
            .as_mut()
            .ok_or(InputError::RegionUnavailable)?
            .pointer_button(message)?;
        let edge = plan_button_edge(&self.held_buttons, code, mapped.pressed);
        let mut events = axis_position_events(mapped.position).to_vec();
        if let Some((edge_code, pressed)) = edge {
            events.push(InputEvent::new(
                EventType::KEY.0,
                edge_code,
                i32::from(pressed),
            ));
        }
        self.absolute_device.emit(&events)?;
        if let Some((edge_code, pressed)) = edge {
            commit_button_edge(&mut self.held_buttons, edge_code, pressed);
        }
        self.stats.mouse_buttons.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn region_pointer_scroll(
        &mut self,
        message: &RegionPointerScrollMsg,
    ) -> Result<(), InputError> {
        let mapped = self
            .region_input
            .as_mut()
            .ok_or(InputError::RegionUnavailable)?
            .pointer_scroll(message)?;
        let mut events = axis_position_events(mapped.position).to_vec();
        let vertical = region_scroll_steps(mapped.delta_y);
        let horizontal = region_scroll_steps(mapped.delta_x);
        if vertical != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL.0,
                vertical,
            ));
        }
        if horizontal != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_HWHEEL.0,
                horizontal,
            ));
        }
        self.absolute_device.emit(&events)?;
        self.stats.scroll_events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Injects one validated `PenEventMsg` sample, moving the cursor and
    /// firing clicks on the absolute-pointer uinput device (a master-POINTER
    /// slave) and optionally forwarding full tablet-tool axes (pressure,
    /// tilt) on the separate tablet-tool uinput device for XI2-aware
    /// applications.
    ///
    /// # Device routing rationale
    ///
    /// Xorg's xf86-input-libinput driver attaches tablet-tool uinput devices
    /// to the master KEYBOARD, not the master POINTER, because the XI2
    /// tablet-tool protocol tracks pen position independently from the core
    /// cursor.  As a result, ABS_X/Y events on the tablet device do **not**
    /// move the screen cursor — they are only visible to XI2 clients that
    /// explicitly subscribe to `LIBINPUT_EVENT_TABLET_TOOL_AXIS`.  For WAN
    /// tablet support we need the cursor to visibly follow the pen and clicks
    /// to land correctly, so this function routes position and pen-tip/barrel
    /// events through `absolute_device` (master POINTER slave, same device
    /// that handles absolute mouse positioning) and uses `tablet_device` only
    /// as a supplementary XI2 pressure/tilt source.
    ///
    /// Callers must call `PenEventMsg::validate()` and advance the shared
    /// input sequence tracker before calling this.
    pub fn pen_event(&mut self, message: &PenEventMsg) -> Result<(), InputError> {
        let axes = pen::PenAxes::from_event(message, self.width, self.height);
        let (edges, new_state) = pen::plan_pen_edges(self.pen_state, message);
        self.inject_pen(PenInjection {
            axes,
            edges,
            new_state,
            pressure: message.pressure,
            tilt_x_degrees: message.tilt_x_degrees,
            tilt_y_degrees: message.tilt_y_degrees,
            tool: message.tool,
            in_proximity: message.in_proximity,
            touching: message.touching,
            sequence: message.sequence,
            source: "legacy",
        })
    }

    pub fn region_pen_event(&mut self, message: &RegionPenEventMsg) -> Result<(), InputError> {
        let mapped = self
            .region_input
            .as_mut()
            .ok_or(InputError::RegionUnavailable)?
            .pen(message)?;
        let axes =
            pen::PenAxes::from_region_sample(&mapped.sample, mapped.position.x, mapped.position.y);
        let (edges, new_state) = pen::plan_region_pen_edges(self.pen_state, &mapped.sample);
        self.inject_pen(PenInjection {
            axes,
            edges,
            new_state,
            pressure: mapped.sample.pressure,
            tilt_x_degrees: mapped.sample.tilt_x_degrees,
            tilt_y_degrees: mapped.sample.tilt_y_degrees,
            tool: pen::wire_pen_tool(mapped.sample.tool),
            in_proximity: mapped.sample.in_proximity,
            touching: mapped.sample.touching,
            sequence: message.metadata.sequence,
            source: "region",
        })
    }

    fn inject_pen(&mut self, injection: PenInjection) -> Result<(), InputError> {
        let PenInjection {
            axes,
            edges,
            new_state,
            pressure,
            tilt_x_degrees,
            tilt_y_degrees,
            tool,
            in_proximity,
            touching,
            sequence,
            source,
        } = injection;
        // Level 3 diagnostic: log every raw injection for E2E tracing and
        // new-device bring-up. Gated at trace level to avoid 100 Hz log spam
        // in normal operation; enable with ARCEN_LOG=input=trace.
        tracing::trace!(
            target: "input",
            x_px = axes.x,
            y_px = axes.y,
            pressure,
            tilt_x = tilt_x_degrees,
            tilt_y = tilt_y_degrees,
            in_proximity,
            touching,
            tool = ?tool,
            seq = sequence,
            source,
            edges = edges.len(),
            "pen_inject: absolute pointer device (cursor+click) + tablet device (XI2)"
        );

        // ── Cursor movement + click via the ABSOLUTE pointer device ──────
        //
        // Routes through the same master-POINTER slave as absolute mouse
        // positioning, guaranteeing the screen cursor actually moves.
        let mut abs_events: Vec<InputEvent> = Vec::new();
        if in_proximity {
            // Keep the cursor at the pen position while the pen is in range.
            abs_events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_X.0,
                axes.x,
            ));
            abs_events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_Y.0,
                axes.y,
            ));
        }
        // Map pen-tool state transitions to pointer buttons on the absolute
        // device.  BTN_TOOL_PEN/RUBBER do not have a mouse equivalent (they
        // are proximity markers, not clickable buttons) and are skipped here;
        // they are still sent to the tablet-tool device below.
        for &(code, pressed) in &edges {
            let mouse_btn = match code {
                pen::BTN_TOUCH => Some(KeyCode::BTN_LEFT.0), // tip contact → left click
                pen::BTN_STYLUS => Some(KeyCode::BTN_RIGHT.0), // lower barrel → right click
                pen::BTN_STYLUS2 => Some(KeyCode::BTN_MIDDLE.0), // upper barrel → middle click
                _ => None,
            };
            if let Some(btn) = mouse_btn {
                abs_events.push(InputEvent::new(EventType::KEY.0, btn, i32::from(pressed)));
                // Mirror into held_buttons so reset_held() releases these if
                // the session drops while a pen button is down.
                commit_button_edge(&mut self.held_buttons, btn, pressed);
            }
        }
        if !abs_events.is_empty() {
            self.absolute_device.emit(&abs_events)?;
        }

        // ── Supplementary XI2 data via the tablet-tool device ────────────
        //
        // Sends BTN_TOOL_PEN proximity markers plus ABS_PRESSURE/TILT so
        // pressure-aware XI2 applications (Krita, GIMP) can read tablet-tool
        // valuators directly.  Best-effort: a failure here is logged at trace
        // but does not abort the event — cursor/click injection above already
        // succeeded.
        if let Some(tablet_device) = self.tablet_device.as_mut() {
            let mut tab_events: Vec<InputEvent> = edges
                .iter()
                .map(|&(code, pressed)| InputEvent::new(EventType::KEY.0, code, i32::from(pressed)))
                .collect();
            tab_events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_X.0,
                axes.x,
            ));
            tab_events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_Y.0,
                axes.y,
            ));
            tab_events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_PRESSURE.0,
                axes.pressure,
            ));
            tab_events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_TILT_X.0,
                axes.tilt_x,
            ));
            tab_events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_TILT_Y.0,
                axes.tilt_y,
            ));
            if let Err(e) = tablet_device.emit(&tab_events) {
                tracing::trace!(
                    target: "input",
                    error = %e,
                    "pen_inject: tablet XI2 emit failed (non-fatal; cursor+click via absolute device succeeded)"
                );
            }
        }

        // Log proximity transitions only (not every event) to confirm the
        // full E2E path without log spam at 100+ Hz pen sample rate.
        let was_in_proximity = self.pen_state.tool.is_some();
        let now_in_proximity = new_state.tool.is_some();
        if !was_in_proximity && now_in_proximity {
            tracing::debug!(
                target: "input",
                "pen entered proximity; cursor via absolute device, XI2 via tablet device"
            );
        } else if was_in_proximity && !now_in_proximity {
            tracing::debug!(target: "input", "pen left proximity");
        }
        self.pen_state = new_state;
        self.stats.pen_events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// True because the pen pipeline always routes through the absolute
    /// pointer device, which is always present.  The tablet-tool device
    /// (supplementary XI2 pressure/tilt) is optional and its absence does
    /// not prevent basic pen operation (cursor movement + clicking).
    #[must_use]
    pub const fn pen_available(&self) -> bool {
        true
    }

    /// Destroys the virtual tablet-tool device so a bridged physical tablet is
    /// the only tablet on the seat. Returns whether a device was released.
    ///
    /// Hard USB attaches the operator's real Wacom to this host, where the
    /// vendor driver claims it and Xorg loads `wacom` for its Pen, eraser,
    /// cursor and Pad nodes. `Arcen Virtual Tablet Pen` is then a *second*
    /// tablet on the same seat, advertising tablet-tool capabilities it will
    /// never emit — in Hard USB the typed pen path is dead by construction,
    /// because capturing the device removes it from macOS and AppKit stops
    /// producing the events that feed [`Self::pen_event`].
    ///
    /// The device cannot simply be left unbuilt instead. `InputController` is
    /// constructed before the client's requested tablet mode is negotiated,
    /// and the same device backs the typed path that negotiation may still
    /// fall back to, so releasing it once the mode resolves is the only
    /// ordering that keeps both modes honest.
    ///
    /// Idempotent, and deliberately does not touch the absolute or relative
    /// pointer devices: mouse and keyboard stay live in every tablet mode.
    pub fn release_tablet_device(&mut self) -> bool {
        if self.tablet_device.is_none() {
            return false;
        }
        // Held proximity/touch/button bits must be dropped *through* the
        // device, before it goes away — a destroyed device cannot retract
        // them, and Xorg would keep the stale press for the seat's lifetime.
        if let Err(error) = self.reset_pen_held() {
            tracing::warn!(
                target: "input",
                error = %error,
                "failed to release held pen state before destroying the virtual tablet device"
            );
        }
        // Dropping `VirtualDevice` closes its `/dev/uinput` file descriptor,
        // and that close is what destroys the kernel device and makes udev and
        // Xorg drop the node.
        self.tablet_device = None;
        true
    }

    /// Whether this attachment owns the shared region-to-Xorg adapter.
    ///
    /// This is runtime truth used by `ServerHello` and by the control
    /// dispatcher: only committed Match My Layout sessions construct it.
    #[must_use]
    pub const fn region_input_available(&self) -> bool {
        self.region_input.is_some()
    }

    fn sync_lock_states(&mut self, message: &KeyEventMsg) -> Result<(), InputError> {
        for (index, requested) in [
            message.caps_lock_on,
            message.num_lock_on,
            message.scroll_lock_on,
        ]
        .into_iter()
        .enumerate()
        {
            let Some(requested) = requested else {
                continue;
            };
            if requested != self.lock_state[index] {
                self.emit_key(LOCK_CODES[index], true)?;
                self.emit_key(LOCK_CODES[index], false)?;
                self.lock_state[index] = requested;
            }
        }
        Ok(())
    }

    fn emit_key(&mut self, code: u16, pressed: bool) -> Result<(), InputError> {
        self.absolute_device.emit(&[InputEvent::new(
            EventType::KEY.0,
            code,
            i32::from(pressed),
        )])?;
        Ok(())
    }

    fn position_events(&self, x: f64, y: f64) -> [InputEvent; 2] {
        [
            InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_X.0,
                pen::normalized_axis(x, self.width),
            ),
            InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_Y.0,
                pen::normalized_axis(y, self.height),
            ),
        ]
    }
}

impl Drop for InputController {
    fn drop(&mut self) {
        if let Err(error) = self.reset_held() {
            tracing::error!(error = %error, "failed to release held uinput state during teardown");
        }
    }
}

/// Builds the separate virtual tablet-tool device: a distinct kernel device
/// from the absolute-mouse device above so Xorg/libinput never classify the
/// plain pointer device as a tablet. `width`/`height` reuse the same fixed
/// X11 raster the absolute-mouse device maps to, so both agree with the
/// compositor's single coordinate mapping.
///
/// Advertises exactly the capability set this backend can prove today: X/Y,
/// pressure (documented 13-bit range), X/Y tilt (whole-degree passthrough),
/// proximity/tool via `BTN_TOOL_PEN`/`BTN_TOOL_RUBBER`, tip contact via
/// `BTN_TOUCH`, and two barrel buttons via `BTN_STYLUS`/`BTN_STYLUS2`.
/// Deliberately omits any rotation axis: no target here has proven the
/// kernel/libinput stack recognizes a chosen axis as tablet rotation, and an
/// unproven axis is worse than the honest `Unavailable` default.
fn build_tablet_device(width: u32, height: u32) -> Result<VirtualDevice, InputError> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in [
        KeyCode(pen::BTN_TOOL_PEN),
        KeyCode(pen::BTN_TOOL_RUBBER),
        KeyCode(pen::BTN_TOUCH),
        KeyCode(pen::BTN_STYLUS),
        KeyCode(pen::BTN_STYLUS2),
    ] {
        keys.insert(code);
    }
    let x_axis = UinputAbsSetup::new(
        AbsoluteAxisCode::ABS_X,
        AbsInfo::new(0, 0, width.saturating_sub(1) as i32, 0, 0, 1),
    );
    let y_axis = UinputAbsSetup::new(
        AbsoluteAxisCode::ABS_Y,
        AbsInfo::new(0, 0, height.saturating_sub(1) as i32, 0, 0, 1),
    );
    let pressure_axis = UinputAbsSetup::new(
        AbsoluteAxisCode::ABS_PRESSURE,
        AbsInfo::new(0, 0, pen::PRESSURE_MAX_13BIT, 0, 0, 0),
    );
    let tilt_x_axis = UinputAbsSetup::new(
        AbsoluteAxisCode::ABS_TILT_X,
        AbsInfo::new(0, pen::TILT_MIN_DEGREES, pen::TILT_MAX_DEGREES, 0, 0, 0),
    );
    let tilt_y_axis = UinputAbsSetup::new(
        AbsoluteAxisCode::ABS_TILT_Y,
        AbsInfo::new(0, pen::TILT_MIN_DEGREES, pen::TILT_MAX_DEGREES, 0, 0, 0),
    );
    let device = VirtualDevice::builder()?
        .name("Arcen Virtual Tablet Pen")
        .input_id(InputId::new(BusType::BUS_VIRTUAL, 0xA2CE, 0x0003, 1))
        .with_keys(&keys)?
        .with_absolute_axis(&x_axis)?
        .with_absolute_axis(&y_axis)?
        .with_absolute_axis(&pressure_axis)?
        .with_absolute_axis(&tilt_x_axis)?
        .with_absolute_axis(&tilt_y_axis)?
        .build()?;
    Ok(device)
}

fn scroll_steps(value: f64) -> i32 {
    if value == 0.0 || !value.is_finite() {
        return 0;
    }
    let rounded = value.round() as i32;
    if rounded == 0 {
        value.signum() as i32
    } else {
        rounded
    }
}

fn region_scroll_steps(value: i64) -> i32 {
    if value == 0 {
        return 0;
    }
    let magnitude = u128::from(value.unsigned_abs());
    let rounded = (magnitude + 60) / 120;
    let bounded = rounded.clamp(1, 2_147_483_647_u128);
    let signed = i32::try_from(bounded).unwrap_or(i32::MAX);
    if value.is_negative() {
        -signed
    } else {
        signed
    }
}

fn mouse_button_code(button: u8) -> Result<u16, InputError> {
    match button {
        1 => Ok(KeyCode::BTN_LEFT.0),
        2 => Ok(KeyCode::BTN_MIDDLE.0),
        3 => Ok(KeyCode::BTN_RIGHT.0),
        button => Err(InputError::InvalidButton(button)),
    }
}

fn axis_position_events(point: XorgAxisPoint) -> [InputEvent; 2] {
    [
        InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, point.x),
        InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, point.y),
    ]
}

const fn prepends_absolute_position(mode: PointerMotionMode) -> bool {
    matches!(mode, PointerMotionMode::Absolute)
}

fn relative_motion_events(dx: i32, dy: i32) -> [InputEvent; 2] {
    [
        InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, dx),
        InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, dy),
    ]
}

fn plan_key_edges(
    held: &mut HashSet<u16>,
    code: u16,
    pressed: bool,
    modifiers: u32,
) -> Vec<(u16, bool)> {
    let mut edges = Vec::with_capacity(5);
    if is_modifier(code) {
        append_idempotent_edge(held, &mut edges, code, pressed);
        return edges;
    }
    for (bit, modifier_code) in [
        (MOD_SHIFT, 42),
        (MOD_CTRL, 29),
        (MOD_ALT, 56),
        (MOD_META, 125),
    ] {
        append_idempotent_edge(held, &mut edges, modifier_code, modifiers & bit != 0);
    }
    append_idempotent_edge(held, &mut edges, code, pressed);
    edges
}

fn append_idempotent_edge(
    held: &mut HashSet<u16>,
    edges: &mut Vec<(u16, bool)>,
    code: u16,
    pressed: bool,
) {
    let changed = if pressed {
        held.insert(code)
    } else {
        held.remove(&code)
    };
    if changed {
        edges.push((code, pressed));
    }
}

fn plan_button_edge(held: &HashSet<u16>, code: u16, pressed: bool) -> Option<(u16, bool)> {
    (held.contains(&code) != pressed).then_some((code, pressed))
}

fn commit_button_edge(held: &mut HashSet<u16>, code: u16, pressed: bool) {
    if pressed {
        held.insert(code);
    } else {
        held.remove(&code);
    }
}

fn plan_reset_releases(held_keys: &HashSet<u16>, held_buttons: &HashSet<u16>) -> Vec<u16> {
    let mut codes: Vec<u16> = held_keys
        .iter()
        .chain(held_buttons.iter())
        .copied()
        .collect();
    for code in MODIFIER_CODES {
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    codes
}

fn plan_keyboard_reset_releases(held_keys: &HashSet<u16>) -> Vec<u16> {
    let mut codes = held_keys.iter().copied().collect::<Vec<_>>();
    for code in MODIFIER_CODES {
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    codes.sort_unstable();
    codes
}

fn is_modifier(code: u16) -> bool {
    MODIFIER_CODES.contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves this module's portable, evdev-independent `pen::BTN_*`
    /// constants (kept dependency-free so `pen.rs` can be unit tested off
    /// Linux) still equal the real `evdev::KeyCode` values used to build the
    /// actual tablet-tool device on Linux, so the two can never silently
    /// drift apart.
    #[test]
    fn pen_key_code_constants_match_evdev_key_codes() {
        assert_eq!(pen::BTN_TOOL_PEN, KeyCode::BTN_TOOL_PEN.0);
        assert_eq!(pen::BTN_TOOL_RUBBER, KeyCode::BTN_TOOL_RUBBER.0);
        assert_eq!(pen::BTN_TOUCH, KeyCode::BTN_TOUCH.0);
        assert_eq!(pen::BTN_STYLUS, KeyCode::BTN_STYLUS.0);
        assert_eq!(pen::BTN_STYLUS2, KeyCode::BTN_STYLUS2.0);
    }

    #[test]
    fn normalized_axes_stay_in_fixed_raster_space_after_stream_resize() {
        assert_eq!(pen::normalized_axis(0.5, 2560), 1280);
        assert_ne!(
            pen::normalized_axis(0.5, 2560),
            pen::normalized_axis(0.5, 1800)
        );
    }

    #[test]
    fn fractional_scroll_preserves_direction() {
        assert_eq!(scroll_steps(0.1), 1);
        assert_eq!(scroll_steps(-0.1), -1);
        assert_eq!(scroll_steps(f64::NAN), 0);
    }

    #[test]
    fn region_scroll_fixed_point_maps_to_signed_wheel_steps() {
        assert_eq!(region_scroll_steps(0), 0);
        assert_eq!(region_scroll_steps(1), 1);
        assert_eq!(region_scroll_steps(120), 1);
        assert_eq!(region_scroll_steps(180), 2);
        assert_eq!(region_scroll_steps(-180), -2);
        assert_eq!(region_scroll_steps(i64::MAX), i32::MAX);
        assert_eq!(region_scroll_steps(i64::MIN), -i32::MAX);
    }

    #[test]
    fn relative_motion_is_one_rel_xy_batch_without_absolute_axes() {
        let events = relative_motion_events(-9, 4);
        assert_eq!(events[0].event_type(), EventType::RELATIVE);
        assert_eq!(events[0].code(), RelativeAxisCode::REL_X.0);
        assert_eq!(events[0].value(), -9);
        assert_eq!(events[1].event_type(), EventType::RELATIVE);
        assert_eq!(events[1].code(), RelativeAxisCode::REL_Y.0);
        assert_eq!(events[1].value(), 4);
    }

    #[test]
    fn relative_edges_and_wheels_omit_absolute_position() {
        assert!(prepends_absolute_position(PointerMotionMode::Absolute));
        assert!(!prepends_absolute_position(PointerMotionMode::Relative));
    }

    #[test]
    fn missing_ctrl_edge_is_synthesized_before_ctrl_a() {
        let mut held = HashSet::new();
        assert_eq!(
            plan_key_edges(&mut held, 30, true, MOD_CTRL),
            vec![(29, true), (30, true)]
        );
    }

    #[test]
    fn explicit_modifier_edges_are_idempotent() {
        let mut held = HashSet::new();
        assert_eq!(
            plan_key_edges(&mut held, 29, true, MOD_CTRL),
            vec![(29, true)]
        );
        assert!(plan_key_edges(&mut held, 29, true, MOD_CTRL).is_empty());
        assert_eq!(plan_key_edges(&mut held, 29, false, 0), vec![(29, false)]);
        assert!(plan_key_edges(&mut held, 29, false, 0).is_empty());
    }

    #[test]
    fn modifier_mask_reconciles_shift_alt_meta_and_releases_stale_state() {
        let mut held = HashSet::from([29]);
        assert_eq!(
            plan_key_edges(&mut held, 2, true, MOD_SHIFT | MOD_ALT | MOD_META),
            vec![(42, true), (29, false), (56, true), (125, true), (2, true)]
        );
    }

    #[test]
    fn cmd_to_ctrl_policy_arrives_as_compact_ctrl_and_synthesizes_ctrl() {
        let mut held = HashSet::new();
        assert_eq!(
            plan_key_edges(&mut held, 31, true, MOD_CTRL),
            vec![(29, true), (31, true)]
        );
    }

    #[test]
    fn drag_focus_loss_releases_held_mouse_button_on_reset() {
        let mut held_keys = HashSet::new();
        let mut held_buttons = HashSet::new();
        assert_eq!(
            plan_button_edge(&held_buttons, KeyCode::BTN_LEFT.0, true),
            Some((KeyCode::BTN_LEFT.0, true))
        );
        commit_button_edge(&mut held_buttons, KeyCode::BTN_LEFT.0, true);
        assert!(plan_button_edge(&held_buttons, KeyCode::BTN_LEFT.0, true).is_none());

        let releases = plan_reset_releases(&held_keys, &held_buttons);
        assert!(releases.contains(&KeyCode::BTN_LEFT.0));
        assert!(held_buttons.contains(&KeyCode::BTN_LEFT.0));
        held_keys.clear();
        held_buttons.clear();
    }

    #[test]
    fn keyboard_reset_never_releases_a_held_mouse_button() {
        let mut held_keys = HashSet::new();
        held_keys.insert(30);
        let mut held_buttons = HashSet::new();
        held_buttons.insert(KeyCode::BTN_LEFT.0);

        let keyboard_releases = plan_keyboard_reset_releases(&held_keys);
        assert!(keyboard_releases.contains(&30));
        assert!(!keyboard_releases.contains(&KeyCode::BTN_LEFT.0));

        let full_releases = plan_reset_releases(&held_keys, &held_buttons);
        assert!(full_releases.contains(&KeyCode::BTN_LEFT.0));
    }

    #[test]
    fn explicit_mouse_release_is_idempotent_and_clears_tracking() {
        let mut held = HashSet::new();
        assert_eq!(
            plan_button_edge(&held, KeyCode::BTN_RIGHT.0, true),
            Some((KeyCode::BTN_RIGHT.0, true))
        );
        commit_button_edge(&mut held, KeyCode::BTN_RIGHT.0, true);
        assert_eq!(
            plan_button_edge(&held, KeyCode::BTN_RIGHT.0, false),
            Some((KeyCode::BTN_RIGHT.0, false))
        );
        commit_button_edge(&mut held, KeyCode::BTN_RIGHT.0, false);
        assert!(plan_button_edge(&held, KeyCode::BTN_RIGHT.0, false).is_none());
        assert!(held.is_empty());
    }
}
