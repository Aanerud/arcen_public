// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Match My Layout's further windows (owner: `ui` stream): one borderless
//! fullscreen window per negotiated monitor beyond the one the root window
//! shows, as the macOS Deck opens one native fullscreen window per display.
//!
//! Which window shows which monitor on which display, when the layout is
//! confirmed and when it must end are the shared engine's decisions
//! (`arcen_deck_core::multi_window`, driven through the launcher's
//! connection); each pass this module opens the windows the plan names,
//! reports what Windows says of every window (fullscreen, the display under
//! it, a close) and, once the layout is committed, presents each monitor's
//! frames through that window's own composition tree and swapchain
//! presenters on the shared D3D11 device, and hands the window's input to
//! the session for its monitor.

use std::sync::Arc;

use arcen_deck_core::multi_window::runtime::{
    ActiveDisplayInfo, ViewportBindObservation, window_title_for,
};
use arcen_deck_core::present::{PresenterPipeline, VideoPresenterSink};
use arcen_media::SessionMonitorId;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowId};

use super::backdrop::Backdrop;
use super::launcher::Launcher;
use super::session::{ROOT_WINDOW, WindowObservation};
use super::{display_number, display_of, hwnd_of};
use crate::present::composition::{CompositionTree, SharedTree};
use crate::present::{D3d11VideoSink, Gpu};

/// One further window of a multi-window session.
struct SecondaryWindow {
    monitor: SessionMonitorId,
    display: u32,
    // Field order is drop order: the presenters before the tree they draw
    // into, the window last.
    eight_bit: D3d11VideoSink,
    ten_bit: D3d11VideoSink,
    tree: SharedTree,
    _backdrop: Backdrop,
    hwnd: windows::Win32::Foundation::HWND,
    close_requested: bool,
    input_attached: bool,
    window: Arc<Window>,
}

impl Drop for SecondaryWindow {
    fn drop(&mut self) {
        self.eight_bit.teardown();
        self.ten_bit.teardown();
    }
}

/// The session's further windows.
#[derive(Default)]
pub(super) struct Secondaries {
    windows: Vec<SecondaryWindow>,
    /// A simulated layout's root window is on its tile (developer builds).
    #[cfg(feature = "dev-tools")]
    root_placed: bool,
}

impl std::fmt::Debug for Secondaries {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Secondaries")
            .field("windows", &self.windows.len())
            .finish_non_exhaustive()
    }
}

/// What a window's state says to the shared engine: bound when it is
/// fullscreen, on the display Windows has it on.
fn observe(window: &Window, close_requested: bool) -> ViewportBindObservation {
    #[allow(clippy::cast_possible_truncation, reason = "a display scale factor")]
    let scale = window.scale_factor() as f32;
    let points = |size: PhysicalSize<u32>| {
        #[allow(clippy::cast_precision_loss, reason = "window sizes in pixels")]
        let size = (size.width as f32 / scale, size.height as f32 / scale);
        size
    };
    let monitor = window.current_monitor();
    ViewportBindObservation {
        inner_rect_known: true,
        fullscreen: Some(window.fullscreen().is_some()),
        close_requested,
        monitor_size_pts: monitor.as_ref().map(|monitor| points(monitor.size())),
        inner_rect_size_pts: Some(points(window.inner_size())),
        observed_display_id: display_of(window),
    }
}

/// The displays attached now, as the shared engine counts them.
fn live_displays(event_loop: &ActiveEventLoop) -> Vec<ActiveDisplayInfo> {
    event_loop
        .available_monitors()
        .filter_map(|monitor| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                reason = "monitor sizes and scales"
            )]
            let (width, height) = {
                let scale = monitor.scale_factor() as f32;
                let size = monitor.size();
                (size.width as f32 / scale, size.height as f32 / scale)
            };
            Some(ActiveDisplayInfo {
                display_id: display_number(&monitor)?,
                width_pts: width,
                height_pts: height,
            })
        })
        .collect()
}

/// A simulated layout's windows (developer builds): each is bound on its
/// synthetic display when it is shown on the display carrying the layout.
#[cfg(feature = "dev-tools")]
fn simulated_observations(
    event_loop: &ActiveEventLoop,
    observations: &mut [WindowObservation],
    root_display: u32,
    planned: &[(SessionMonitorId, u32)],
) {
    for observation in observations {
        let expected = if observation.key == ROOT_WINDOW {
            root_display
        } else {
            planned
                .iter()
                .find(|(monitor, _)| monitor.get() == observation.key)
                .map_or(root_display, |(_, display)| *display)
        };
        observation.observation =
            super::windowed_test::observation(event_loop, observation.observation, expected);
    }
}

/// How `monitor`'s window opens: borderless fullscreen on its display, or
/// (a simulated layout, developer builds) an ordinary window at its tile.
/// `None` when its display is not attached.
fn attributes_for(
    event_loop: &ActiveEventLoop,
    monitor: SessionMonitorId,
    screen: u32,
) -> Option<(winit::window::WindowAttributes, bool)> {
    #[cfg(feature = "dev-tools")]
    if let Some(layout) = super::windowed_test::layout() {
        return super::windowed_test::window_attributes(
            event_loop,
            layout,
            monitor,
            window_title_for(monitor),
        )
        .map(|attributes| (attributes, false));
    }
    let target = monitor_for(event_loop, screen)?;
    Some((
        Window::default_attributes()
            .with_title(window_title_for(monitor))
            .with_decorations(false)
            .with_fullscreen(Some(Fullscreen::Borderless(Some(target)))),
        true,
    ))
}

fn monitor_for(event_loop: &ActiveEventLoop, display: u32) -> Option<MonitorHandle> {
    event_loop
        .available_monitors()
        .find(|monitor| display_number(monitor) == Some(display))
}

impl Secondaries {
    /// Whether `id` is one of these windows.
    pub(super) fn owns(&self, id: WindowId) -> bool {
        self.windows.iter().any(|window| window.window.id() == id)
    }

    /// A window event for one of these windows.
    pub(super) fn window_event(&mut self, id: WindowId, event: &WindowEvent) {
        let Some(window) = self
            .windows
            .iter_mut()
            .find(|window| window.window.id() == id)
        else {
            return;
        };
        match event {
            // Closing one window of the layout ends the layout (the
            // shared teardown rule), never just that window.
            WindowEvent::CloseRequested => window.close_requested = true,
            WindowEvent::Resized(size) => {
                if let Ok(mut tree) = window.tree.lock() {
                    let _ = tree.set_client_size((size.width, size.height));
                    let _ = tree.commit();
                }
            }
            WindowEvent::Occluded(occluded) => {
                window.eight_bit.set_surface_visible(!occluded);
                window.ten_bit.set_surface_visible(!occluded);
            }
            _ => {}
        }
    }

    /// Closes every further window.
    pub(super) fn close_all(&mut self) {
        if !self.windows.is_empty() {
            tracing::info!(
                target: "arcen::display",
                windows = self.windows.len(),
                "closing the multi-window session's further windows",
            );
        }
        self.windows.clear();
        #[cfg(feature = "dev-tools")]
        {
            self.root_placed = false;
        }
    }

    /// One pass of a multi-window session: open the plan's windows, report
    /// every window to the shared engine, then, once committed, give each
    /// window its input and its monitor's frames.
    pub(super) fn drive(
        &mut self,
        event_loop: &ActiveEventLoop,
        root: &Window,
        gpu: &Gpu,
        launcher: &mut Launcher,
        ctx: &egui::Context,
    ) {
        let Some((root_display, planned)) = launcher.multi_window_plan() else {
            self.close_all();
            return;
        };
        #[cfg(feature = "dev-tools")]
        if let Some(layout) = super::windowed_test::layout()
            && !self.root_placed
        {
            super::windowed_test::place_root(event_loop, layout, root);
            self.root_placed = true;
        }
        self.windows.retain(|window| {
            planned
                .iter()
                .any(|(monitor, display)| *monitor == window.monitor && *display == window.display)
        });
        for (monitor, screen) in &planned {
            if self.windows.iter().any(|window| window.monitor == *monitor) {
                continue;
            }
            // A display that is not attached gets no window: the engine
            // sees it missing and ends the layout, never a window on the
            // wrong display.
            let Some((attributes, fullscreen)) = attributes_for(event_loop, *monitor, *screen)
            else {
                continue;
            };
            match Self::open(event_loop, gpu, *monitor, *screen, attributes, fullscreen) {
                Ok(window) => self.windows.push(window),
                Err(error) => tracing::error!(
                    target: "arcen::display",
                    %error,
                    monitor = monitor.get(),
                    display_id = *screen,
                    "could not open a multi-window session's window",
                ),
            }
        }

        let (observations, live) = self.engine_view(event_loop, root, root_display, &planned);
        let any_close = self.windows.iter().any(|window| window.close_requested);
        launcher.observe_multi_window(&observations, &live, any_close, ctx);

        let Some(connection) = launcher.connection() else {
            self.close_all();
            return;
        };
        if !connection.multi_window_committed() {
            return;
        }
        let frames = connection.take_secondary_frames();
        let mut attach = Vec::new();
        for window in &mut self.windows {
            if !window.input_attached {
                attach.push((window.hwnd, window.monitor));
                window.input_attached = true;
            }
            connection.sync_secondary_input(window.hwnd, window.monitor, &window.tree);
        }
        for (monitor, pipeline, frame) in &frames {
            let Some(window) = self
                .windows
                .iter_mut()
                .find(|window| window.monitor == *monitor)
            else {
                continue;
            };
            let outcome = match pipeline {
                PresenterPipeline::EightBit => window.eight_bit.present(frame),
                PresenterPipeline::TenBit => window.ten_bit.present(frame),
            };
            if outcome.is_presented() {
                connection.note_secondary_presented(*monitor, 1);
            }
        }
        for (hwnd, monitor) in attach {
            launcher.attach_secondary_input(hwnd, monitor);
        }
        if !frames.is_empty() {
            ctx.request_repaint();
        }
    }

    /// What the shared engine sees this pass: every window and the
    /// displays attached now (a simulated layout's own, in developer
    /// builds).
    fn engine_view(
        &self,
        event_loop: &ActiveEventLoop,
        root: &Window,
        root_display: u32,
        planned: &[(SessionMonitorId, u32)],
    ) -> (Vec<WindowObservation>, Vec<ActiveDisplayInfo>) {
        let mut observations = vec![WindowObservation {
            key: ROOT_WINDOW,
            observation: observe(root, false),
        }];
        observations.extend(self.windows.iter().map(|window| WindowObservation {
            key: window.monitor.get(),
            observation: observe(&window.window, window.close_requested),
        }));
        #[cfg(feature = "dev-tools")]
        if let Some(layout) = super::windowed_test::layout() {
            simulated_observations(event_loop, &mut observations, root_display, planned);
            return (
                observations,
                super::windowed_test::live_displays(event_loop, layout),
            );
        }
        let _ = (root_display, planned);
        (observations, live_displays(event_loop))
    }

    fn open(
        event_loop: &ActiveEventLoop,
        gpu: &Gpu,
        monitor: SessionMonitorId,
        screen: u32,
        attributes: winit::window::WindowAttributes,
        fullscreen: bool,
    ) -> Result<SecondaryWindow, String> {
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| format!("cannot create the window: {error}"))?,
        );
        let hwnd = hwnd_of(&window)?;
        let backdrop = Backdrop::install(hwnd)?;
        let mut tree = CompositionTree::new(gpu, hwnd)?;
        let size = window.inner_size();
        let _ = tree.set_client_size((size.width, size.height));
        tree.set_fullscreen(fullscreen);
        tree.commit()?;
        let tree: SharedTree = Arc::new(std::sync::Mutex::new(tree));
        tracing::info!(
            target: "arcen::display",
            monitor = monitor.get(),
            display_id = screen,
            width = size.width,
            height = size.height,
            "opened a multi-window session's window",
        );
        Ok(SecondaryWindow {
            monitor,
            display: screen,
            eight_bit: D3d11VideoSink::new(
                PresenterPipeline::EightBit,
                gpu.clone(),
                Arc::clone(&tree),
            ),
            ten_bit: D3d11VideoSink::new(PresenterPipeline::TenBit, gpu.clone(), Arc::clone(&tree)),
            tree,
            _backdrop: backdrop,
            hwnd,
            close_requested: false,
            input_attached: false,
            window,
        })
    }
}
