// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The window (owner: `ui` stream).
//!
//! A winit window with the native Win32 menu bar (the Mac Deck uses the
//! macOS menu bar); the present module's `DirectComposition` tree on its
//! client area, above the window's own surface (video, cursor, overlay); egui-winit
//! for input and egui-wgpu (DX12) drawing the shared screens
//! (`arcen-deck-core` feature `egui`) into the overlay visual (ADR 0024).
//! The window is sized and titled like the macOS Deck's, and the egui
//! context names the machine "this PC" (`DeckPlatform::Pc`).

#![allow(
    unsafe_code,
    reason = "the overlay surface is created from the composition tree's raw IDCompositionVisual"
)]

mod backdrop;
mod displays;
mod launcher;
mod menu_bar;
mod overlay;
mod secondary;
mod session;
#[cfg(feature = "dev-tools")]
mod windowed_test;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arcen_deck_core::config::ConnectionKind;
use arcen_deck_core::connection_draft::ConnectionDraft;
use arcen_deck_core::display::SessionFullscreenStep;
use arcen_deck_core::password_text::read_password_text;
use arcen_deck_core::quick_connect::parse_quick_connect;
use arcen_deck_core::runtime::tls::{TlsTrustConfig, parse_fingerprint};
use arcen_deck_core::screens::wording::{DeckPlatform, set_deck_platform};
use arcen_deck_core::ui_pacing::PassPacer;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{
    ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop, EventLoopBuilder, EventLoopProxy,
};
use winit::platform::windows::{MonitorHandleExtWindows, WindowAttributesExtWindows};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

use crate::present::composition::{CompositionTree, OverlayRegion, SharedTree};
use crate::present::{AdapterChoice, Gpu};

/// The window title, as on the Mac.
const TITLE: &str = "Arcen Deck";

/// Wakes the event loop from egui's repaint requests (any thread).
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    Repaint(Duration),
    /// The media worker published decoded frames ([`MediaWake`]).
    Media,
}

/// The loop's proxy for [`MediaWake`], set once the loop exists.
static MEDIA_PROXY: std::sync::OnceLock<std::sync::Mutex<EventLoopProxy<UserEvent>>> =
    std::sync::OnceLock::new();

/// A [`UserEvent::Media`] is on its way and not yet handled.
static MEDIA_WAKE_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The media worker's repaint requester, the macOS Deck's `MediaWake`:
/// egui tells the loop about a repaint only when it is sooner than the one
/// already pending, and a pass is pending while the pacer holds it, so a
/// further window's frame would wait for the root window's next pass (one
/// per refresh, in that pass's phase, the newer frame replacing the older).
/// The worker's own event wakes the loop at once; `about_to_wait` hands
/// the further windows their frames without a UI pass.
pub(super) struct MediaWake(pub(super) egui::Context);

impl arcen_deck_core::present::RepaintRequester for MediaWake {
    fn request_repaint(&self) {
        self.0.request_repaint_of(egui::ViewportId::ROOT);
        if MEDIA_WAKE_PENDING.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        if let Some(proxy) = MEDIA_PROXY.get()
            && let Ok(proxy) = proxy.lock()
            && proxy.send_event(UserEvent::Media).is_err()
        {
            MEDIA_WAKE_PENDING.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

/// What `--connect` asks the window to do once it is open.
struct QuickStart {
    draft: ConnectionDraft,
    stream_stats: bool,
    quit_after: Option<Duration>,
}

/// The quick-connect request on the command line, with its password read.
fn quick_start(args: &[String]) -> Result<Option<QuickStart>, String> {
    let Some(request) = parse_quick_connect(args)? else {
        return Ok(None);
    };
    let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
    draft.host = request.host;
    draft.port = request.port;
    draft.username = request.username;
    if let Some(path) = request.password_file {
        let file = crate::platform::password_file::open_password_file(std::path::Path::new(&path))
            .map_err(|refusal| format!("password file refused: {refusal}"))?;
        draft.password = read_password_text(file).map_err(|error| error.to_string())?;
    }
    if let Some(pin) = request.pin_sha256 {
        let fingerprint = parse_fingerprint(&pin).map_err(|error| error.to_string())?;
        draft.tls_trust = Some(TlsTrustConfig::pinned(fingerprint));
    }
    Ok(Some(QuickStart {
        draft,
        stream_stats: request.stream_stats,
        quit_after: request.quit_after_secs.map(Duration::from_secs),
    }))
}

/// Runs the Deck's window until it closes. Returns the process exit code.
#[must_use]
pub fn run(args: &[String]) -> ExitCode {
    let quick = match quick_start(args) {
        Ok(quick) => quick,
        Err(error) => {
            eprintln!("arcen-deck: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("arcen-deck")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("arcen-deck: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Sessions spawn their tasks from the UI thread, as on the Mac.
    let _runtime = runtime.enter();
    let event_loop = match build_event_loop(&mut EventLoop::<UserEvent>::with_user_event()) {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("arcen-deck: cannot create the window's event loop: {error}");
            return ExitCode::FAILURE;
        }
    };
    let platform = Arc::new(crate::platform::WindowsDeckServices::new());
    let services: Arc<dyn arcen_deck_core::DeckServices> = platform.clone();
    let _log_file = crate::logging::init(&*services, arcen_deck_core::logging::startup_profile());
    crate::platform::prepare_process();
    let _ = MEDIA_PROXY.set(std::sync::Mutex::new(event_loop.create_proxy()));
    let mut shell = Shell {
        proxy: event_loop.create_proxy(),
        services,
        platform,
        window: None,
        failure: None,
        quick,
        unattended_end: None,
        secondaries: secondary::Secondaries::default(),
    };
    let result = event_loop.run_app(&mut shell);
    if let Some(failure) = shell.failure {
        eprintln!("arcen-deck: {failure}");
        return ExitCode::FAILURE;
    }
    if shell.unattended_end == Some(launcher::UnattendedEnd::Failed) {
        return ExitCode::FAILURE;
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("arcen-deck: {error}");
            ExitCode::FAILURE
        }
    }
}

struct Shell {
    proxy: EventLoopProxy<UserEvent>,
    services: Arc<dyn arcen_deck_core::DeckServices>,
    /// The same services, for what only this shell asks of them.
    platform: Arc<crate::platform::WindowsDeckServices>,
    window: Option<DeckWindow>,
    failure: Option<String>,
    quick: Option<QuickStart>,
    unattended_end: Option<launcher::UnattendedEnd>,
    /// Match My Layout's further windows.
    secondaries: secondary::Secondaries,
}

/// One Deck window: its composition tree, overlay renderer and egui.
struct DeckWindow {
    // Field order is drop order: the renderer's surface goes before the
    // tree that owns its visual, and the window goes last.
    overlay: overlay::OverlayRenderer,
    /// Shared with the video sinks of a session's media (they draw into its
    /// video visual).
    tree: SharedTree,
    /// The device of the session's decoder and presenters (the root
    /// window's; Match My Layout's further windows present on it too).
    gpu: Gpu,
    egui: egui_winit::State,
    menu_bar: menu_bar::MenuBar,
    backdrop: backdrop::Backdrop,
    launcher: launcher::Launcher,
    /// What the overlay's last presented pass covers.
    overlay_region: OverlayRegion,
    /// What the overlay's last presented pass drew.
    overlay_shown: Option<ShownPass>,
    repaint_at: Option<Instant>,
    /// Passes never begin sooner than one refresh of the window's display
    /// apart (`arcen_deck_core::ui_pacing::PassPacer`).
    pacer: PassPacer,
    refresh_period: Option<Duration>,
    /// The display the window was last noted on (`\\.\DISPLAYn`'s `n`).
    display: Option<u32>,
    window: Arc<Window>,
}

/// One refresh of the display under most of the window.
fn refresh_period_of(window: &Window) -> Option<Duration> {
    let millihertz = window.current_monitor()?.refresh_rate_millihertz()?;
    (millihertz > 0).then(|| Duration::from_secs_f64(1_000.0 / f64::from(millihertz)))
}

/// The roster id of the display under most of the window.
fn display_of(window: &Window) -> Option<u32> {
    display_number(&window.current_monitor()?)
}

/// A monitor's roster id: `\\.\DISPLAY3` is 3.
fn display_number(monitor: &winit::monitor::MonitorHandle) -> Option<u32> {
    crate::display::gdi_display_number(&monitor.native_id())
}

/// The shell's event loop, registered for no raw input.
///
/// winit registers every event loop for raw keyboard and mouse input (its
/// device events). Windows calls no `WH_KEYBOARD_LL` hook, this Deck's or
/// any other program's, while the newest hook belongs to a process
/// registered for raw keyboard input whose window is in front: the Deck's
/// own keyboard hook then took nothing, and a Windows Pier keeping its desk
/// private while this Deck ran in its session lost its hook canary and,
/// rightly, ended and locked the session. The Deck reads keys from its
/// window procedure and its hook, and raw mouse motion only while the
/// pointer is locked (`input::mouse`); it uses no device event.
fn build_event_loop(
    builder: &mut EventLoopBuilder<UserEvent>,
) -> Result<EventLoop<UserEvent>, winit::error::EventLoopError> {
    let event_loop = builder.build()?;
    event_loop.listen_device_events(DeviceEvents::Never);
    Ok(event_loop)
}

fn hwnd_of(window: &Window) -> Result<windows::Win32::Foundation::HWND, String> {
    let handle = window
        .window_handle()
        .map_err(|error| format!("window handle: {error}"))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(windows::Win32::Foundation::HWND(
            handle.hwnd.get() as *mut std::ffi::c_void
        )),
        _ => Err("not a Win32 window".to_owned()),
    }
}

impl Shell {
    fn open_window(&self, event_loop: &ActiveEventLoop) -> Result<DeckWindow, String> {
        let attributes = Window::default_attributes()
            .with_title(TITLE)
            .with_inner_size(LogicalSize::new(1120.0, 760.0))
            .with_min_inner_size(LogicalSize::new(860.0, 600.0))
            // Not `WS_EX_NOREDIRECTIONBITMAP`: user32 draws the menu bar
            // into the window's redirection surface.
            .with_no_redirection_bitmap(false);
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| format!("cannot create the window: {error}"))?,
        );
        let hwnd = hwnd_of(&window)?;
        let choice = window
            .current_monitor()
            .map_or(AdapterChoice::HighPerformance, |monitor| {
                AdapterChoice::Monitor(monitor.hmonitor())
            });
        let wake = self.proxy.clone();
        let mut menu_bar = menu_bar::MenuBar::install(hwnd, move || {
            let _ = wake.send_event(UserEvent::Repaint(Duration::ZERO));
        })?;
        menu_bar.set_session_active(false);
        let backdrop = backdrop::Backdrop::install(hwnd)?;
        let gpu = Gpu::create(&choice)?;
        let tree = CompositionTree::new(&gpu, hwnd)?;
        let size = window.inner_size();
        let overlay =
            overlay::OverlayRenderer::new(tree.overlay_visual_raw(), (size.width, size.height))?;
        tree.commit()?;
        let tree: SharedTree = Arc::new(std::sync::Mutex::new(tree));

        let ctx = egui::Context::default();
        arcen_deck_core::screens::theme::apply(&ctx);
        set_deck_platform(&ctx, DeckPlatform::Pc);
        let proxy = self.proxy.clone();
        ctx.set_request_repaint_callback(move |info| {
            let _ = proxy.send_event(UserEvent::Repaint(info.delay));
        });
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a display scale factor is a small number"
        )]
        let scale = window.scale_factor() as f32;
        let egui = egui_winit::State::new(
            ctx,
            egui::ViewportId::ROOT,
            &window,
            Some(scale),
            None,
            Some(overlay.max_texture_side()),
        );
        let launcher = launcher::Launcher::new(
            Arc::clone(&self.services),
            crate::media::VideoSurface {
                gpu: gpu.clone(),
                tree: Arc::clone(&tree),
            },
            hwnd,
        );
        Ok(DeckWindow {
            overlay,
            tree,
            gpu,
            egui,
            menu_bar,
            backdrop,
            launcher,
            overlay_region: OverlayRegion::default(),
            overlay_shown: None,
            repaint_at: Some(Instant::now()),
            pacer: PassPacer::default(),
            refresh_period: refresh_period_of(&window),
            display: None,
            window,
        })
    }
}

/// The egui pass the overlay shows.
struct ShownPass {
    shapes: Vec<egui::epaint::ClippedShape>,
    pixels_per_point: f32,
    client: (u32, u32),
}

impl DeckWindow {
    fn paint(&mut self) {
        let raw = self.egui.take_egui_input(&self.window);
        let ctx = self.egui.egui_ctx().clone();
        let launcher = &mut self.launcher;
        let output = ctx.run_ui(raw, |ui| launcher.ui(ui));
        self.menu_bar.set_session_active(self.launcher.in_session());
        self.menu_bar
            .set_keyboard_status(&self.launcher.keyboard_status_line());
        self.menu_bar
            .set_switch_chord(self.launcher.keyboard_switch());
        self.backdrop.set_fill(if self.launcher.in_session() {
            egui::Color32::BLACK
        } else {
            ctx.global_style().visuals.panel_fill
        });
        self.egui
            .handle_platform_output(&self.window, output.platform_output);
        let size = self.window.inner_size();
        let client = (size.width, size.height);
        // A pass that draws what the overlay already shows is not rendered
        // or presented: the session repaints with every video frame, and
        // every overlay present, even of an unchanged or empty picture,
        // moves DWM's deadline for the video to the overlay's present.
        let unchanged = output.textures_delta.is_empty()
            && self.overlay_shown.as_ref().is_some_and(|shown| {
                shown.client == client
                    && shown.pixels_per_point.to_bits() == output.pixels_per_point.to_bits()
                    && shown.shapes == output.shapes
            });
        let painted = unchanged || {
            let shapes = output.shapes.clone();
            let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
            // The overlay visual covers only what egui drew (out of the tree
            // when nothing): a full-window overlay brings DWM's deadline
            // forward.
            let region = OverlayRegion::of(&primitives, output.pixels_per_point, client);
            if let Ok(mut tree) = self.tree.lock() {
                let _ = tree.set_overlay_region(region.union(self.overlay_region));
            }
            let painted =
                self.overlay
                    .paint(&primitives, &output.textures_delta, output.pixels_per_point);
            if painted {
                if let Ok(mut tree) = self.tree.lock() {
                    let _ = tree.set_overlay_region(region);
                }
                self.overlay_region = region;
                self.overlay_shown = Some(ShownPass {
                    shapes,
                    pixels_per_point: output.pixels_per_point,
                    client,
                });
            } else {
                self.overlay_shown = None;
            }
            painted
        };
        if !painted {
            // The pass's picture was not shown (surface busy, occluded or
            // reconfigured): draw it again rather than wait for input.
            self.repaint_at = Some(Instant::now() + Duration::from_millis(50));
            return;
        }
        self.repaint_at = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.repaint_delay)
            .filter(|delay| *delay < Duration::from_secs(3600))
            .map(|delay| Instant::now() + delay);
    }

    /// Fullscreen on display `display` (`\\.\DISPLAYn`'s `n`; the
    /// window's own display when it is not attached): where a pinned
    /// session's stream was sized for, so the picture is 1:1.
    fn set_fullscreen_on(&mut self, display: Option<u32>) {
        let target = display.and_then(|display| {
            self.window
                .available_monitors()
                .find(|monitor| display_number(monitor) == Some(display))
        });
        let monitor = target.or_else(|| self.window.current_monitor());
        self.apply_fullscreen(true, monitor);
        // A session that went fullscreen by itself is in front: Windows
        // keeps the taskbar over a fullscreen window that is not.
        self.window.focus_window();
    }

    /// Fullscreen on the window's display (borderless, as the Mac's
    /// fullscreen space), without the menu bar; or back.
    fn set_fullscreen(&mut self, fullscreen: bool) {
        let monitor = self.window.current_monitor();
        self.apply_fullscreen(fullscreen, monitor);
    }

    fn apply_fullscreen(
        &mut self,
        fullscreen: bool,
        monitor: Option<winit::monitor::MonitorHandle>,
    ) {
        if let Ok(mut tree) = self.tree.lock() {
            tree.set_fullscreen(fullscreen);
        }
        self.menu_bar.set_shown(!fullscreen);
        self.window
            .set_fullscreen(fullscreen.then_some(winit::window::Fullscreen::Borderless(monitor)));
        self.launcher.set_fullscreen(fullscreen);
        self.repaint_at = Some(Instant::now());
    }

    fn resized(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        self.overlay.resize((size.width, size.height));
        // A reconfigured surface holds no picture (minimise and restore
        // come back at the shown size): the next pass presents again.
        self.overlay_shown = None;
        if let Ok(mut tree) = self.tree.lock() {
            let _ = tree.set_client_size((size.width, size.height));
            let _ = tree.commit();
        }
        self.window.request_redraw();
    }
}

/// Tells the services which display the window is on when that changes.
fn note_display(platform: &crate::platform::WindowsDeckServices, window: &mut DeckWindow) {
    let display = display_of(&window.window);
    if display.is_some() && display != window.display {
        window.display = display;
        window.launcher.note_window_display(display);
        if let Some(id) = display {
            platform.note_session_display(id);
        }
    }
}

impl ApplicationHandler<UserEvent> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        match self.open_window(event_loop) {
            Ok(mut window) => {
                // Before the first hello: decode facts follow this
                // display's adapter.
                note_display(&self.platform, &mut window);
                if let Some(quick) = self.quick.take() {
                    let ctx = window.egui.egui_ctx().clone();
                    window.launcher.quick_connect(
                        quick.draft,
                        quick.stream_stats,
                        quick.quit_after,
                        &ctx,
                    );
                }
                window.window.request_redraw();
                self.window = Some(window);
            }
            Err(error) => {
                self.failure = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.secondaries.owns(window_id) {
            self.secondaries.window_event(window_id, &event);
            return;
        }
        let Some(window) = self.window.as_mut() else {
            return;
        };
        let response = window.egui.on_window_event(&window.window, &event);
        match event {
            WindowEvent::CloseRequested => {
                self.secondaries.close_all();
                self.window = None;
                event_loop.exit();
                return;
            }
            WindowEvent::Resized(size) => {
                // Minimised: no presents until the window shows again.
                window
                    .launcher
                    .set_visible(size.width > 0 && size.height > 0);
                window.pacer.release();
                window.resized(size);
            }
            WindowEvent::Occluded(occluded) => window.launcher.set_visible(!occluded),
            WindowEvent::Moved(_) => {
                window.refresh_period = refresh_period_of(&window.window);
                note_display(&self.platform, window);
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                window.pacer.release();
                window.refresh_period = refresh_period_of(&window.window);
                note_display(&self.platform, window);
            }
            WindowEvent::RedrawRequested => {
                // egui asks for two more passes after every input pass: a
                // pass asked for sooner than one refresh after the last is
                // held until then and carries what arrived meanwhile.
                if window.pacer.admit(Instant::now(), window.refresh_period) {
                    window.paint();
                } else if let Some(due) = window.pacer.held_until(window.refresh_period) {
                    window.repaint_at = Some(window.repaint_at.map_or(due, |at| at.min(due)));
                }
            }
            _ => {}
        }
        if response.repaint {
            window.window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        let delay = match event {
            UserEvent::Repaint(delay) => delay,
            UserEvent::Media => {
                // `about_to_wait` follows and hands out the frames.
                MEDIA_WAKE_PENDING.store(false, std::sync::atomic::Ordering::Release);
                return;
            }
        };
        if let Some(window) = self.window.as_mut() {
            let at = Instant::now() + delay;
            window.repaint_at = Some(window.repaint_at.map_or(at, |current| current.min(at)));
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.as_mut() else {
            return;
        };
        let mut commands = menu_bar::drain();
        commands.extend(window.launcher.take_bar_commands());
        for command in commands {
            match window.launcher.menu_command(command) {
                launcher::MenuOutcome::Handled => window.repaint_at = Some(Instant::now()),
                launcher::MenuOutcome::ToggleFullscreen => {
                    window.set_fullscreen(window.window.fullscreen().is_none());
                }
                launcher::MenuOutcome::Exit => {
                    self.window = None;
                    event_loop.exit();
                    return;
                }
            }
        }
        // A pinned session (Primary Display Only, Match My Layout) is
        // fullscreen on the display its stream was sized for, as on the Mac.
        let fullscreen = window.window.fullscreen().is_some();
        match window.launcher.session_fullscreen_step(fullscreen) {
            Some((SessionFullscreenStep::Enter, display)) => window.set_fullscreen_on(display),
            Some((SessionFullscreenStep::Leave { restore }, _)) if restore != fullscreen => {
                window.set_fullscreen(restore);
            }
            _ => {}
        }
        // Match My Layout: one further window per monitor.
        let ctx = window.egui.egui_ctx().clone();
        self.secondaries.drive(
            event_loop,
            &window.window,
            &window.gpu,
            &mut window.launcher,
            &ctx,
        );
        // Fullscreen is a session's: back to the window when it ends.
        if !window.launcher.in_session() && window.window.fullscreen().is_some() {
            window.set_fullscreen(false);
        }
        if window.launcher.exit_requested() {
            self.window = None;
            event_loop.exit();
            return;
        }
        if let Some(end) = window.launcher.unattended_end() {
            self.unattended_end = Some(end);
            self.window = None;
            event_loop.exit();
            return;
        }
        if window.launcher.unattended() {
            // Wake at least once a second to report and to end on time.
            let soon = Instant::now() + Duration::from_secs(1);
            window.repaint_at = Some(window.repaint_at.map_or(soon, |at| at.min(soon)));
        }
        match window.repaint_at {
            Some(at) if at <= Instant::now() => {
                window.repaint_at = None;
                window.window.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::Input::{GetRegisteredRawInputDevices, RAWINPUTDEVICE};
    use winit::platform::windows::EventLoopBuilderExtWindows;

    use super::{EventLoop, UserEvent, build_event_loop};

    /// The process's raw input registrations (usage page, usage).
    fn registered_raw_input() -> Vec<(u16, u16)> {
        let size = u32::try_from(std::mem::size_of::<RAWINPUTDEVICE>()).unwrap_or(0);
        let mut count = 0_u32;
        // SAFETY: a size query: no buffer, a valid count out-parameter.
        let _ = unsafe { GetRegisteredRawInputDevices(None, &raw mut count, size) };
        let mut devices = vec![RAWINPUTDEVICE::default(); count as usize];
        if count > 0 {
            // SAFETY: a buffer of `count` elements and its length.
            let read = unsafe {
                GetRegisteredRawInputDevices(Some(devices.as_mut_ptr()), &raw mut count, size)
            };
            devices.truncate(usize::try_from(read).unwrap_or(0).min(devices.len()));
        }
        devices
            .iter()
            .map(|device| (device.usUsagePage, device.usUsage))
            .collect()
    }

    #[test]
    fn the_event_loop_leaves_the_process_without_raw_keyboard_input() -> Result<(), String> {
        let mut builder = EventLoop::<UserEvent>::with_user_event();
        builder.with_any_thread(true);
        let event_loop = build_event_loop(&mut builder).map_err(|error| error.to_string())?;
        let registered = registered_raw_input();
        drop(event_loop);
        assert!(
            !registered.contains(&(0x01, 0x06)),
            "raw keyboard input registered: {registered:?}"
        );
        Ok(())
    }
}
