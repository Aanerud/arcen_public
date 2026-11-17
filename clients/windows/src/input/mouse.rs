// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The mouse: absolute motion and buttons in message order, the wheel, and
//! the pointer lock (raw input while the cursor is clipped and hidden).

use arcen_input::ScrollPhase;
use arcen_input::deck_pen::ImageRect;
use arcen_input::deck_wheel::{WheelAccumulator, WheelAxis, WheelScroll};
use arcen_protocol::messages::ScrollUnitMsg;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTDEVICE_FLAGS, RAWINPUTHEADER, RID_INPUT, RIDEV_REMOVE, RIM_TYPEMOUSE,
    RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, GetClientRect, GetCursorPos, GetMessageExtraInfo, ShowCursor, WM_INPUT,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SIZE, WM_XBUTTONDBLCLK, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use super::InputEvent;

/// `GetMessageExtraInfo` marks mouse messages Windows made from a pen or a
/// touch with this signature in its upper bits.
const PEN_OR_TOUCH_SIGNATURE: isize = 0xFF51_5700;
const SIGNATURE_MASK: isize = 0xFFFF_FF00;

/// The session window's mouse.
#[derive(Debug, Default)]
pub(super) struct Mouse {
    /// Wire buttons held (bit 1 left, 2 middle, 3 right).
    buttons: u8,
    wheel: WheelAccumulator,
    locked: bool,
    last: (f64, f64),
    /// The client point of the last move sent.
    last_point: Option<(i32, i32)>,
}

impl Mouse {
    pub(super) fn set_precise_scroll(&mut self, precise: bool) {
        self.wheel = WheelAccumulator::new(precise);
    }

    pub(super) const fn locked(&self) -> bool {
        self.locked
    }

    /// A button press went to the host and is not released yet.
    pub(super) const fn buttons_held(&self) -> bool {
        self.buttons != 0
    }

    /// Where a pointer message happened, in client pixels (a wheel message
    /// carries screen coordinates; the others client ones).
    pub(super) fn message_point(hwnd: HWND, msg: u32, lparam: LPARAM) -> Option<(f64, f64)> {
        if matches!(msg, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
            let mut point = POINT {
                x: screen_x(lparam),
                y: screen_y(lparam),
            };
            // SAFETY: converts a point of this window's screen.
            let placed = unsafe { ScreenToClient(hwnd, &raw mut point) }.as_bool();
            return placed.then(|| (f64::from(point.x), f64::from(point.y)));
        }
        Some((client_x(lparam), client_y(lparam)))
    }

    pub(super) fn handle(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        image: ImageRect,
        mut emit: impl FnMut(InputEvent),
    ) -> Option<LRESULT> {
        let handled = Some(LRESULT(0));
        match msg {
            WM_MOUSEMOVE => {
                if self.locked || from_pen_or_touch() {
                    return handled;
                }
                // Windows repeats WM_MOUSEMOVE where the mouse did not move
                // (a pen leaving, the cursor shown again); sending it would
                // pull the host pointer back from where the pen left it.
                let point = (screen_x(lparam), screen_y(lparam));
                if self.last_point == Some(point) {
                    return handled;
                }
                self.last_point = Some(point);
                if let Some((x, y)) = self.locate(image, client_x(lparam), client_y(lparam)) {
                    self.last = (x, y);
                    emit(InputEvent::PointerMove { x, y });
                }
                handled
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP | WM_RBUTTONDOWN
            | WM_RBUTTONDBLCLK | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONDBLCLK
            | WM_MBUTTONUP => {
                if from_pen_or_touch() {
                    return handled;
                }
                let (button, pressed) = match msg {
                    WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => (1, true),
                    WM_LBUTTONUP => (1, false),
                    WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => (2, true),
                    WM_MBUTTONUP => (2, false),
                    WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => (3, true),
                    _ => (3, false),
                };
                if !self.locked
                    && let Some(position) = self.locate(image, client_x(lparam), client_y(lparam))
                {
                    self.last = position;
                }
                self.button(hwnd, button, pressed, &mut emit);
                handled
            }
            // The wire has no side buttons yet (no Pier injects them); kept
            // from the Deck's system too, as the macOS Deck does.
            WM_XBUTTONDOWN | WM_XBUTTONUP | WM_XBUTTONDBLCLK => Some(LRESULT(1)),
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "the high word is the delta"
                )]
                let delta = i32::from(((wparam.0 >> 16) as u16).cast_signed());
                let axis = if msg == WM_MOUSEWHEEL {
                    WheelAxis::Vertical
                } else {
                    WheelAxis::Horizontal
                };
                if !self.locked {
                    let mut point = POINT {
                        x: screen_x(lparam),
                        y: screen_y(lparam),
                    };
                    // SAFETY: converts a point of this window's screen.
                    let _ = unsafe { ScreenToClient(hwnd, &raw mut point) };
                    if let Some(position) =
                        self.locate(image, f64::from(point.x), f64::from(point.y))
                    {
                        self.last = position;
                    }
                }
                if let Some(scroll) = self.wheel.wheel(axis, delta) {
                    emit(InputEvent::Scroll {
                        scroll,
                        x: self.last.0,
                        y: self.last.1,
                    });
                }
                handled
            }
            WM_INPUT => {
                if self.locked
                    && let Some((dx, dy)) = relative_motion(lparam)
                {
                    emit(InputEvent::PointerRelative { dx, dy });
                }
                // The default procedure frees the raw input buffer.
                None
            }
            WM_SIZE | WM_MOVE => {
                if self.locked {
                    clip_to_client(hwnd);
                }
                None
            }
            _ => None,
        }
    }

    /// Where a client point is on the picture: clamped to it, except while
    /// a button is held. Then this window has the capture and reports where
    /// the cursor really is, past its own edges, so the session can follow
    /// the drag into the window under the cursor
    /// (`arcen_deck_core::input::pointer_drag`).
    fn locate(&self, image: ImageRect, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.buttons == 0 {
            return image.normalize(x, y);
        }
        // `normalize` refuses a degenerate rectangle or a non-finite point.
        image.normalize(x, y)?;
        Some((
            (x - image.left) / image.width,
            (y - image.top) / image.height,
        ))
    }

    /// A touchpad's scroll of `dx`/`dy` host pixels (signed as a wheel) at
    /// the system cursor: precise and phased to a host that takes it,
    /// gathered into notches for others.
    pub(super) fn touchpad_scroll(
        &mut self,
        hwnd: HWND,
        image: ImageRect,
        (dx, dy): (f64, f64),
        phase: ScrollPhase,
        mut emit: impl FnMut(InputEvent),
    ) {
        if !self.locked {
            let mut point = POINT::default();
            // SAFETY: plain reads into a local point.
            let placed = unsafe {
                GetCursorPos(&raw mut point).is_ok()
                    && ScreenToClient(hwnd, &raw mut point).as_bool()
            };
            if placed
                && let Some(position) = image.normalize(f64::from(point.x), f64::from(point.y))
            {
                self.last = position;
            }
        }
        let (x, y) = self.last;
        if self.wheel.precise_host() {
            emit(InputEvent::Scroll {
                scroll: WheelScroll {
                    dx,
                    dy,
                    unit: ScrollUnitMsg::Point,
                    phase,
                },
                x,
                y,
            });
            return;
        }
        // A host without precise scroll: a pixel is a 120th of a notch, as
        // the macOS Deck gathers a trackpad for such a host.
        for (axis, value) in [(WheelAxis::Vertical, dy), (WheelAxis::Horizontal, dx)] {
            #[allow(clippy::cast_possible_truncation, reason = "rounded, one gesture step")]
            let delta = value.round() as i32;
            if let Some(scroll) = self.wheel.wheel(axis, delta) {
                emit(InputEvent::Scroll { scroll, x, y });
            }
        }
    }

    /// Whether the system cursor is over the image.
    pub(super) fn over(hwnd: HWND, image: ImageRect) -> bool {
        let mut point = POINT::default();
        // SAFETY: plain reads into a local point.
        let placed = unsafe {
            GetCursorPos(&raw mut point).is_ok() && ScreenToClient(hwnd, &raw mut point).as_bool()
        };
        placed
            && f64::from(point.x) >= image.left
            && f64::from(point.y) >= image.top
            && f64::from(point.x) < image.left + image.width
            && f64::from(point.y) < image.top + image.height
    }

    /// Releases every button held on the host (focus loss).
    pub(super) fn release_buttons(&mut self, mut emit: impl FnMut(InputEvent)) {
        for button in 1..=3 {
            if self.buttons & (1 << button) != 0 {
                emit(InputEvent::Button {
                    button,
                    pressed: false,
                    x: self.last.0,
                    y: self.last.1,
                });
            }
        }
        if self.buttons != 0 {
            self.buttons = 0;
            // SAFETY: gives up a capture this window may hold.
            let _ = unsafe { ReleaseCapture() };
        }
        self.wheel.clear();
    }

    /// Locks the pointer: raw relative motion, the cursor hidden and kept
    /// inside the window.
    pub(super) fn lock(&mut self, hwnd: HWND) {
        if self.locked {
            return;
        }
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RAWINPUTDEVICE_FLAGS::default(),
            hwndTarget: hwnd,
        };
        // SAFETY: one well-formed device registration for this window.
        let registered = unsafe {
            RegisterRawInputDevices(
                &[device],
                u32::try_from(std::mem::size_of::<RAWINPUTDEVICE>()).unwrap_or(0),
            )
        };
        if registered.is_err() {
            return;
        }
        clip_to_client(hwnd);
        // SAFETY: balanced by `unlock`.
        let _ = unsafe { ShowCursor(false) };
        self.locked = true;
    }

    /// Releases the pointer lock; whether one was held.
    pub(super) fn unlock(&mut self) -> bool {
        if !self.locked {
            return false;
        }
        self.locked = false;
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        };
        // SAFETY: removes the registration `lock` made; frees the clip and
        // shows the cursor `lock` hid.
        unsafe {
            let _ = RegisterRawInputDevices(
                &[device],
                u32::try_from(std::mem::size_of::<RAWINPUTDEVICE>()).unwrap_or(0),
            );
            let _ = ClipCursor(None);
            let _ = ShowCursor(true);
        }
        true
    }

    fn button(&mut self, hwnd: HWND, button: u8, pressed: bool, emit: &mut impl FnMut(InputEvent)) {
        let bit = 1 << button;
        if (self.buttons & bit != 0) == pressed {
            return;
        }
        if pressed {
            if self.buttons == 0 {
                // SAFETY: drags that leave the window keep their release.
                let _ = unsafe { SetCapture(hwnd) };
            }
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
            if self.buttons == 0 {
                // SAFETY: gives up the capture taken on the first press.
                let _ = unsafe { ReleaseCapture() };
            }
        }
        emit(InputEvent::Button {
            button,
            pressed,
            x: self.last.0,
            y: self.last.1,
        });
    }
}

impl Drop for Mouse {
    fn drop(&mut self) {
        // A lock never outlives the window's input: the cursor comes back.
        self.unlock();
    }
}

fn from_pen_or_touch() -> bool {
    // SAFETY: a plain read of the current message's extra information.
    unsafe { GetMessageExtraInfo() }.0 & SIGNATURE_MASK == PEN_OR_TOUCH_SIGNATURE
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "GET_X_LPARAM"
)]
fn screen_x(lparam: LPARAM) -> i32 {
    i32::from(((lparam.0 & 0xFFFF) as u16).cast_signed())
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "GET_Y_LPARAM"
)]
fn screen_y(lparam: LPARAM) -> i32 {
    i32::from((((lparam.0 >> 16) & 0xFFFF) as u16).cast_signed())
}

fn client_x(lparam: LPARAM) -> f64 {
    f64::from(screen_x(lparam))
}

fn client_y(lparam: LPARAM) -> f64 {
    f64::from(screen_y(lparam))
}

/// The window's client area and `image` in it, in screen pixels: where
/// this window is on the Deck's desktop, for a drag that leaves it.
pub(super) fn screen_placement(hwnd: HWND, image: ImageRect) -> Option<(ImageRect, ImageRect)> {
    let mut client = RECT::default();
    let mut origin = POINT::default();
    // SAFETY: reads this window's client rectangle and maps its origin to
    // the screen.
    let placed = unsafe {
        GetClientRect(hwnd, &raw mut client).is_ok()
            && ClientToScreen(hwnd, &raw mut origin).as_bool()
    };
    if !placed || client.right <= client.left || client.bottom <= client.top {
        return None;
    }
    let (left, top) = (f64::from(origin.x), f64::from(origin.y));
    let window = ImageRect {
        left,
        top,
        width: f64::from(client.right - client.left),
        height: f64::from(client.bottom - client.top),
    };
    let picture = ImageRect {
        left: left + image.left,
        top: top + image.top,
        ..image
    };
    Some((window, picture))
}

fn clip_to_client(hwnd: HWND) {
    let mut rect = RECT::default();
    // SAFETY: reads this window's client rectangle and clips the cursor to
    // it, in screen coordinates.
    unsafe {
        if GetClientRect(hwnd, &raw mut rect).is_err() {
            return;
        }
        let mut top_left = POINT {
            x: rect.left,
            y: rect.top,
        };
        let mut bottom_right = POINT {
            x: rect.right,
            y: rect.bottom,
        };
        let _ = ClientToScreen(hwnd, &raw mut top_left);
        let _ = ClientToScreen(hwnd, &raw mut bottom_right);
        let screen = RECT {
            left: top_left.x,
            top: top_left.y,
            right: bottom_right.x,
            bottom: bottom_right.y,
        };
        let _ = ClipCursor(Some(&raw const screen));
    }
}

/// A raw mouse report's relative motion, if it has any.
fn relative_motion(lparam: LPARAM) -> Option<(i32, i32)> {
    let mut raw = RAWINPUT::default();
    let mut size = u32::try_from(std::mem::size_of::<RAWINPUT>()).ok()?;
    let header = u32::try_from(std::mem::size_of::<RAWINPUTHEADER>()).ok()?;
    // SAFETY: `raw` is large enough for a mouse report (`size` says so);
    // the handle is this message's.
    let read = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut core::ffi::c_void),
            RID_INPUT,
            Some(std::ptr::from_mut(&mut raw).cast()),
            &raw mut size,
            header,
        )
    };
    if read == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    // SAFETY: the header says this is a mouse report.
    let mouse = unsafe { raw.data.mouse };
    if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
        return None;
    }
    (mouse.lLastX != 0 || mouse.lLastY != 0).then_some((mouse.lLastX, mouse.lLastY))
}
