// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The joined console's windows put back after a Deck's layout (ADR 0025
//! §6, GAP-046 closed): the Linux adapter.
//!
//! The decision is `arcen_outputs::display_lease::plan_window_restore`'s.
//! This module reads the session's top-level windows through EWMH
//! (`_NET_CLIENT_LIST`, each window's position on the root, its size, and
//! its `_NET_WM_STATE` maximised and fullscreen states) and moves a window
//! back with `_NET_MOVERESIZE_WINDOW` (static gravity, from a pager) and
//! `_NET_WM_STATE`. These are public on every EWMH window manager. The
//! connection is the console's own X server, opened as root with the
//! session's cookie (`console::connect`). Desktop and dock windows are not
//! read: they belong to the shell, which places them itself.
//!
//! Measured on the lab console (GNOME Shell 40.10, Xorg, NVIDIA): mutter
//! itself returns a window to its monitor when that monitor comes back, so
//! the restore found every window in place there. The moves themselves were
//! proved behind the lock screen (`a_window_is_moved_back_on_a_live_display`,
//! a maximised window re-maximised on its own monitor). A window manager
//! that does not return windows gets the same result from this module.

use arcen_outputs::display_lease::{DesktopArea, WindowPlacement};
use serde::{Deserialize, Serialize};
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

/// The most windows read: more is not a desktop a person arranged.
const MAX_WINDOWS: u32 = 512;
/// How long the window manager is given to act on one request before the
/// next for the same window.
const STEP_PAUSE: std::time::Duration = std::time::Duration::from_millis(150);

/// A window's placement as journalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedWindow {
    id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
    fullscreen: bool,
}

impl SavedWindow {
    fn placement(self) -> WindowPlacement {
        WindowPlacement {
            id: u64::from(self.id),
            area: DesktopArea::new(self.x, self.y, self.width, self.height),
            maximized: self.maximized,
            fullscreen: self.fullscreen,
        }
    }
}

/// The atoms this module reads and sends.
struct Atoms {
    client_list: xproto::Atom,
    wm_state: xproto::Atom,
    maximized_vert: xproto::Atom,
    maximized_horz: xproto::Atom,
    fullscreen: xproto::Atom,
    window_type: xproto::Atom,
    type_desktop: xproto::Atom,
    type_dock: xproto::Atom,
    moveresize: xproto::Atom,
}

impl Atoms {
    fn intern(connection: &RustConnection) -> Result<Self, String> {
        let atom = |name: &[u8]| -> Result<xproto::Atom, String> {
            connection
                .intern_atom(false, name)
                .map_err(|error| format!("request atom: {error}"))?
                .reply()
                .map(|reply| reply.atom)
                .map_err(|error| format!("atom: {error}"))
        };
        Ok(Self {
            client_list: atom(b"_NET_CLIENT_LIST")?,
            wm_state: atom(b"_NET_WM_STATE")?,
            maximized_vert: atom(b"_NET_WM_STATE_MAXIMIZED_VERT")?,
            maximized_horz: atom(b"_NET_WM_STATE_MAXIMIZED_HORZ")?,
            fullscreen: atom(b"_NET_WM_STATE_FULLSCREEN")?,
            window_type: atom(b"_NET_WM_WINDOW_TYPE")?,
            type_desktop: atom(b"_NET_WM_WINDOW_TYPE_DESKTOP")?,
            type_dock: atom(b"_NET_WM_WINDOW_TYPE_DOCK")?,
            moveresize: atom(b"_NET_MOVERESIZE_WINDOW")?,
        })
    }
}

fn atoms_of(
    connection: &RustConnection,
    window: xproto::Window,
    property: xproto::Atom,
    kind: xproto::AtomEnum,
) -> Vec<u32> {
    connection
        .get_property(false, window, property, kind, 0, MAX_WINDOWS)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| reply.value32().map(Iterator::collect))
        .unwrap_or_default()
}

/// The session's top-level windows, in the window manager's order.
///
/// # Errors
///
/// When the window manager publishes no client list (none runs).
pub(crate) fn read(
    connection: &RustConnection,
    root: xproto::Window,
) -> Result<Vec<SavedWindow>, String> {
    let atoms = Atoms::intern(connection)?;
    let reply = connection
        .get_property(
            false,
            root,
            atoms.client_list,
            xproto::AtomEnum::WINDOW,
            0,
            MAX_WINDOWS,
        )
        .map_err(|error| format!("request client list: {error}"))?
        .reply()
        .map_err(|error| format!("client list: {error}"))?;
    let Some(clients) = reply.value32() else {
        return Err("the window manager publishes no client list".to_owned());
    };
    let mut windows = Vec::new();
    for window in clients {
        let kinds = atoms_of(
            connection,
            window,
            atoms.window_type,
            xproto::AtomEnum::ATOM,
        );
        if kinds
            .iter()
            .any(|kind| *kind == atoms.type_desktop || *kind == atoms.type_dock)
        {
            continue;
        }
        let Some(geometry) = connection
            .get_geometry(window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            continue;
        };
        let Some(origin) = connection
            .translate_coordinates(window, root, 0, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            continue;
        };
        let state = atoms_of(connection, window, atoms.wm_state, xproto::AtomEnum::ATOM);
        windows.push(SavedWindow {
            id: window,
            x: i32::from(origin.dst_x),
            y: i32::from(origin.dst_y),
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
            maximized: state.contains(&atoms.maximized_vert)
                && state.contains(&atoms.maximized_horz),
            fullscreen: state.contains(&atoms.fullscreen),
        });
    }
    Ok(windows)
}

/// The monitors' areas on the root (`RandR` 1.5 monitors).
fn monitors(connection: &RustConnection, root: xproto::Window) -> Vec<DesktopArea> {
    connection
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| {
            reply
                .monitors
                .iter()
                .map(|monitor| {
                    DesktopArea::new(
                        i32::from(monitor.x),
                        i32::from(monitor.y),
                        u32::from(monitor.width),
                        u32::from(monitor.height),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn client_message(
    connection: &RustConnection,
    root: xproto::Window,
    window: xproto::Window,
    kind: xproto::Atom,
    data: [u32; 5],
) -> Result<(), String> {
    let event = xproto::ClientMessageEvent::new(32, window, kind, data);
    connection
        .send_event(
            false,
            root,
            xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .map_err(|error| format!("send: {error}"))?;
    connection.flush().map_err(|error| error.to_string())
}

/// `_NET_WM_STATE` actions.
const STATE_REMOVE: u32 = 0;
const STATE_ADD: u32 = 1;
/// `_NET_MOVERESIZE_WINDOW`: static gravity (the client's own origin), x,
/// y, width and height given, from a pager.
const MOVERESIZE_FLAGS: u32 = 10 | (0b1111 << 8) | (2 << 12);
/// The source indication of a `_NET_WM_STATE` request: a pager.
const SOURCE_PAGER: u32 = 2;

fn set_state(
    connection: &RustConnection,
    root: xproto::Window,
    atoms: &Atoms,
    window: xproto::Window,
    action: u32,
    first: xproto::Atom,
    second: xproto::Atom,
) -> Result<(), String> {
    client_message(
        connection,
        root,
        window,
        atoms.wm_state,
        [action, first, second, SOURCE_PAGER, 0],
    )
}

/// Moves one window to `target`: out of maximised or fullscreen first (a
/// window manager does not move those), to the recorded client area, then
/// back into the recorded state, which the window manager applies on the
/// monitor the window is now on.
async fn place(
    connection: &RustConnection,
    root: xproto::Window,
    atoms: &Atoms,
    target: &WindowPlacement,
    current: Option<&SavedWindow>,
) -> Result<(), String> {
    let window = xproto::Window::try_from(target.id).map_err(|error| error.to_string())?;
    let (maximized, fullscreen) =
        current.map_or((false, false), |now| (now.maximized, now.fullscreen));
    if fullscreen {
        set_state(
            connection,
            root,
            atoms,
            window,
            STATE_REMOVE,
            atoms.fullscreen,
            0,
        )?;
    }
    if maximized {
        set_state(
            connection,
            root,
            atoms,
            window,
            STATE_REMOVE,
            atoms.maximized_vert,
            atoms.maximized_horz,
        )?;
    }
    if maximized || fullscreen {
        tokio::time::sleep(STEP_PAUSE).await;
    }
    #[allow(clippy::cast_sign_loss, reason = "X coordinates travel as CARD32")]
    let data = [
        MOVERESIZE_FLAGS,
        target.area.x as u32,
        target.area.y as u32,
        target.area.width,
        target.area.height,
    ];
    client_message(connection, root, window, atoms.moveresize, data)?;
    if target.maximized || target.fullscreen {
        tokio::time::sleep(STEP_PAUSE).await;
    }
    if target.maximized {
        set_state(
            connection,
            root,
            atoms,
            window,
            STATE_ADD,
            atoms.maximized_vert,
            atoms.maximized_horz,
        )?;
    }
    if target.fullscreen {
        set_state(
            connection,
            root,
            atoms,
            window,
            STATE_ADD,
            atoms.fullscreen,
            0,
        )?;
    }
    Ok(())
}

/// How long the window manager is given to take the console's own layout
/// back before its windows are read.
const LAYOUT_SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Puts back every window the Deck's layout moved and nobody moved since
/// (shared `plan_window_restore`), once the console's own layout is back.
/// `before` and `after_layout` are journalled; `departing` was read as the
/// Deck left, still in its layout.
pub(crate) async fn restore(
    connection: &RustConnection,
    root: xproto::Window,
    session_id: &str,
    before: &[SavedWindow],
    after_layout: Option<&[SavedWindow]>,
    departing: &[SavedWindow],
) {
    use crate::logging::target;
    if before.is_empty() {
        return;
    }
    tokio::time::sleep(LAYOUT_SETTLE).await;
    let (now, atoms) = match (read(connection, root), Atoms::intern(connection)) {
        (Ok(now), Ok(atoms)) => (now, atoms),
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(target: target::SESSION, session_id, %error, "the console's windows could not be read; none is moved back");
            return;
        }
    };
    let placements = |list: &[SavedWindow]| -> Vec<WindowPlacement> {
        list.iter().copied().map(SavedWindow::placement).collect()
    };
    let before_placed = placements(before);
    let after_placed = after_layout.map(placements);
    let departing_placed = placements(departing);
    let now_placed = placements(&now);
    let displays = monitors(connection, root);
    let plan = arcen_outputs::display_lease::plan_window_restore(
        &arcen_outputs::display_lease::WindowRestoreInput {
            before: &before_placed,
            after_layout: after_placed.as_deref(),
            departing: &departing_placed,
            now: &now_placed,
            displays_now: &displays,
        },
    );
    let mut moved = 0_usize;
    for target in &plan {
        let current = now.iter().find(|window| u64::from(window.id) == target.id);
        match place(connection, root, &atoms, target, current).await {
            Ok(()) => moved += 1,
            Err(error) => {
                tracing::warn!(target: target::SESSION, session_id, window = target.id, %error, "a window could not be moved back")
            }
        }
    }
    if plan.is_empty() {
        tracing::info!(
            target: target::SESSION,
            session_id,
            windows = now.len(),
            "the console's windows are where they were"
        );
        return;
    }
    tokio::time::sleep(STEP_PAUSE * 2).await;
    let placed = read(connection, root)
        .map(|after| {
            plan.iter()
                .filter(|target| after.iter().any(|window| window.placement() == **target))
                .count()
        })
        .unwrap_or(0);
    tracing::info!(
        target: target::SESSION,
        session_id,
        windows = now.len(),
        asked = moved,
        in_place = placed,
        "the console's windows are moved back where they were before the Deck's layout"
    );
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Lab proof against a live window manager (ignored): moves the window
    /// `ARCEN_LAB_WINDOW` (hex id) on `DISPLAY` to `ARCEN_LAB_PLACE`
    /// (`x,y,width,height,maximized`) and reads it back.
    #[test]
    #[ignore = "needs a live X display with an EWMH window manager"]
    fn a_window_is_moved_back_on_a_live_display() {
        let id = std::env::var("ARCEN_LAB_WINDOW").expect("ARCEN_LAB_WINDOW");
        let id = u32::from_str_radix(id.trim_start_matches("0x"), 16).expect("hex id");
        let spec: Vec<i64> = std::env::var("ARCEN_LAB_PLACE")
            .expect("ARCEN_LAB_PLACE")
            .split(',')
            .map(|part| part.parse().expect("number"))
            .collect();
        let target = WindowPlacement {
            id: u64::from(id),
            area: DesktopArea::new(
                i32::try_from(spec[0]).expect("x"),
                i32::try_from(spec[1]).expect("y"),
                u32::try_from(spec[2]).expect("width"),
                u32::try_from(spec[3]).expect("height"),
            ),
            maximized: spec[4] != 0,
            fullscreen: false,
        };
        let (connection, screen) = x11rb::connect(None).expect("display");
        let root = connection.setup().roots[screen].root;
        let before = read(&connection, root).expect("windows");
        let current = before.iter().find(|window| window.id == id).copied();
        println!("before: {current:?}");
        let atoms = Atoms::intern(&connection).expect("atoms");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(async {
                place(&connection, root, &atoms, &target, current.as_ref())
                    .await
                    .expect("place");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            });
        let after = read(&connection, root).expect("windows");
        let now = after.iter().find(|window| window.id == id).copied();
        println!("after: {now:?}");
        assert_eq!(now.map(SavedWindow::placement), Some(target));
    }
}
