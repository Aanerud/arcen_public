// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The host's cursor image as a native cursor at the display's backing scale.
//!
//! egui hands a custom cursor to winit, whose macOS cursor is an `NSImage`
//! with a single bitmap of one pixel per point. On a Retina display that
//! bitmap is drawn twice as large by the window server, so a host cursor
//! shown at its size in the video (often half a point per host pixel) loses
//! half its detail: thin brush outlines and crosshairs turn soft. This builds
//! the same cursor — same point size, same hotspot — from a bitmap at the
//! backing scale and makes it current while the pointer is over the remote
//! view. winit's own (1x) cursor stays registered for the view's cursor rect,
//! so whenever AppKit reapplies that rect the next UI pass sets the sharp one
//! again; the two differ only in sharpness, never in size or hotspot.

use arcen_media::cursor::PresentedCursor;
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_app_kit::{NSBitmapImageRep, NSCursor, NSDeviceRGBColorSpace, NSImage};
use objc2_foundation::{NSPoint, NSSize};

/// A host cursor drawn at the backing scale.
#[derive(Debug)]
pub(crate) struct SharpCursor {
    cursor: Retained<NSCursor>,
}

impl SharpCursor {
    /// The sharp form of `image` shown at `points_per_pixel` points per
    /// stream pixel on a display of `backing_scale`. `None` (winit's 1x
    /// cursor is used) when the backing-scale bitmap would have to be
    /// reduced by the presentation cap: the two would then differ in size
    /// and hotspot, and the cursor must never change size when AppKit
    /// reapplies the view's cursor rect.
    pub(crate) fn for_image(
        image: &arcen_media::cursor::CursorImage,
        points_per_pixel: f32,
        backing_scale: f32,
    ) -> Option<Self> {
        let device = points_per_pixel * backing_scale;
        if !image.presents_unreduced(device) {
            return None;
        }
        Self::new(&image.present(device), f64::from(backing_scale))
    }

    /// Builds the cursor from `presented`, premultiplied RGBA at
    /// `backing_scale` device pixels per point. `None` when AppKit refuses
    /// the bitmap or the scale is not above one (winit's cursor is already
    /// exact then).
    pub(crate) fn new(presented: &PresentedCursor, backing_scale: f64) -> Option<Self> {
        if !(backing_scale.is_finite() && backing_scale > 1.0) {
            return None;
        }
        let (width, height) = (usize::from(presented.width), usize::from(presented.height));
        if width == 0 || height == 0 || presented.rgba_premultiplied.len() != width * height * 4 {
            return None;
        }
        // SAFETY: a null `planes` asks AppKit to allocate the bitmap, sized
        // from the dimensions passed (8-bit RGBA, rows of `width * 4`).
        let bitmap = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                isize::try_from(width).ok()?,
                isize::try_from(height).ok()?,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                isize::try_from(width * 4).ok()?,
                32,
            )
        }?;
        let data = bitmap.bitmapData();
        if data.is_null() {
            return None;
        }
        // SAFETY: AppKit allocated `width * height * 4` bytes for this
        // representation (checked equal to the source length above).
        unsafe {
            std::ptr::copy_nonoverlapping(
                presented.rgba_premultiplied.as_ptr(),
                data,
                presented.rgba_premultiplied.len(),
            );
        }
        #[allow(clippy::cast_precision_loss)]
        let points = NSSize::new(width as f64 / backing_scale, height as f64 / backing_scale);
        bitmap.setSize(points);
        let image = NSImage::initWithSize(NSImage::alloc(), points);
        image.addRepresentation(&bitmap);
        let hotspot = NSPoint::new(
            f64::from(presented.hotspot.0) / backing_scale,
            f64::from(presented.hotspot.1) / backing_scale,
        );
        Some(Self {
            cursor: NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, hotspot),
        })
    }

    /// Makes this the application's cursor unless it already is.
    pub(crate) fn show(&self) {
        if objc2_foundation::MainThreadMarker::new().is_none() {
            return;
        }
        if NSCursor::currentCursor() != self.cursor {
            self.cursor.set();
        }
    }
}

/// What the system cursor must do this pass while the host may lead the
/// Deck's cursor (`arcen_input::host_pointer::PointerAuthority`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeadCursorChange {
    /// Nothing to change.
    Keep,
    /// Make the transparent cursor current (idempotent, every pass it is
    /// wanted: AppKit may reapply a cursor rect at any time).
    Hide,
    /// Make a visible cursor current again, once.
    Restore,
}

/// Hides the system cursor over the video while the host leads, and gives
/// it back exactly once when it stops.
///
/// It never uses `[NSCursor hide]` (a counter that a missed `unhide` leaves
/// stuck): hiding makes a transparent cursor current, restoring makes the
/// arrow current, and the view's own cursor rect (winit's, re-armed by
/// egui's icon change) or the sharp host cursor takes over from there.
/// winit applies `CursorIcon::None` only through cursor rects, which AppKit
/// re-evaluates on mouse motion; while a bridged tablet leads the Mac mouse
/// does not move, so that alone never hid the cursor.
#[derive(Debug, Default)]
pub(crate) struct LeadCursor {
    hidden: bool,
}

/// What the viewer wants of the system cursor this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeadCursorWant {
    /// A visible cursor: the Mac's pointer leads, or it is not over the
    /// video of a focused window.
    Visible,
    /// The host leads: the cursor is drawn at the host's position, so the
    /// system cursor must not show.
    HostLeads,
    /// The viewer shows no cursor of its own (pointer lock, host-drawn
    /// cursor, a host that hides its pointer). Its egui icon is already
    /// `None`, so egui-winit and winit never re-apply the view's cursor
    /// rect: an arrow set here would stay. A cursor already transparent
    /// stays so; otherwise winit's own hiding is left alone.
    Invisible,
}

impl LeadCursor {
    /// The change for this pass.
    pub(crate) fn update(&mut self, want: LeadCursorWant) -> LeadCursorChange {
        match (self.hidden, want) {
            (_, LeadCursorWant::HostLeads) | (true, LeadCursorWant::Invisible) => {
                self.hidden = true;
                LeadCursorChange::Hide
            }
            (true, LeadCursorWant::Visible) => {
                self.hidden = false;
                LeadCursorChange::Restore
            }
            (false, LeadCursorWant::Visible | LeadCursorWant::Invisible) => LeadCursorChange::Keep,
        }
    }

    /// Whether the transparent cursor is in use.
    pub(crate) const fn is_hidden(&self) -> bool {
        self.hidden
    }
}

thread_local! {
    static TRANSPARENT: Option<SharpCursor> = SharpCursor::new(
        &PresentedCursor {
            rgba_premultiplied: std::sync::Arc::from(vec![0u8; 2 * 2 * 4]),
            width: 2,
            height: 2,
            hotspot: (0, 0),
        },
        2.0,
    );
}

/// Where this Mac's own pointer is, read from AppKit each UI pass.
///
/// egui knows the pointer only from the last window event it saw, and a
/// bridged tablet never moves the Mac pointer: after the session window
/// covers the screen (a frame change under a resting pointer) egui's
/// position is stale or gone, so a hover test built on it can keep the
/// Mac's arrow on screen while the host leads, and a window-relative
/// position that shifts reads as Mac motion. AppKit's global location
/// changes only when the Mac's own pointer moves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MacPointer {
    /// Global location, AppKit screen points.
    pub(crate) screen: (f64, f64),
    /// This application is the active one (never hide over other apps).
    pub(crate) app_active: bool,
    /// The location in the root window's content view (top-left origin,
    /// AppKit points), only when the root window is the topmost window at
    /// the pointer: not another application's window, a Deck panel or the
    /// menu bar.
    pub(crate) in_root_view: Option<(f64, f64)>,
}

/// Reads [`MacPointer`] (main thread only; `None` elsewhere). Tests never
/// read AppKit (`ArcenApp::mac_pointer`).
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn probe_mac_pointer() -> Option<MacPointer> {
    use objc2_app_kit::{NSApplication, NSEvent, NSWindow};
    let mtm = objc2_foundation::MainThreadMarker::new()?;
    let screen = NSEvent::mouseLocation();
    let app_active = NSApplication::sharedApplication(mtm).isActive();
    let in_root_view = super::video_render::find_root_window(mtm).and_then(|window| {
        if !window.isVisible() {
            return None;
        }
        let topmost = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(screen, 0, mtm);
        if topmost != window.windowNumber() {
            return None;
        }
        let view = window.contentView()?;
        let point = view.convertPoint_fromView(window.convertPointFromScreen(screen), None);
        let y = if view.isFlipped() {
            point.y
        } else {
            view.bounds().size.height - point.y
        };
        Some((point.x, y))
    });
    Some(MacPointer {
        screen: (screen.x, screen.y),
        app_active,
        in_root_view,
    })
}

/// Tells this Mac's own pointer motion from window changes.
#[derive(Debug, Default)]
pub(crate) struct MacPointerMotion {
    last: Option<(f64, f64)>,
}

impl MacPointerMotion {
    /// Whether the Mac's own pointer moved since the last reading; the
    /// first reading is a baseline.
    pub(crate) fn moved(&mut self, pointer: &MacPointer) -> bool {
        let moved = self.last.is_some_and(|last| last != pointer.screen);
        self.last = Some(pointer.screen);
        moved
    }

    /// A new session: the next reading is a baseline again.
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }
}

/// What the viewer wants of the system cursor this pass: `on_viewer` (this
/// Mac's pointer is over the root viewer of the active app, nothing of the
/// Deck's own above it), `host_leads` (a host-led cursor is drawn there),
/// and `viewer_shows_no_cursor` (the viewer's egui icon is `None`).
pub(crate) const fn lead_cursor_want(
    on_viewer: bool,
    host_leads: bool,
    viewer_shows_no_cursor: bool,
) -> LeadCursorWant {
    if !on_viewer {
        LeadCursorWant::Visible
    } else if host_leads {
        LeadCursorWant::HostLeads
    } else if viewer_shows_no_cursor {
        LeadCursorWant::Invisible
    } else {
        LeadCursorWant::Visible
    }
}

/// Applies `change` to the application's cursor (main thread only).
pub(crate) fn apply_lead_cursor(change: LeadCursorChange) {
    if objc2_foundation::MainThreadMarker::new().is_none() {
        return;
    }
    match change {
        LeadCursorChange::Keep => {}
        LeadCursorChange::Hide => TRANSPARENT.with(|cursor| {
            if let Some(cursor) = cursor {
                cursor.show();
            }
        }),
        LeadCursorChange::Restore => NSCursor::arrowCursor().set(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::cursor::CursorImage;

    #[test]
    fn the_lead_cursor_hides_while_wanted_and_restores_exactly_once() {
        use LeadCursorWant::{HostLeads, Visible};
        let mut lead = LeadCursor::default();
        assert_eq!(lead.update(Visible), LeadCursorChange::Keep);
        assert_eq!(lead.update(HostLeads), LeadCursorChange::Hide);
        // Re-applied every pass it is wanted (a cursor rect may have won).
        assert_eq!(lead.update(HostLeads), LeadCursorChange::Hide);
        assert!(lead.is_hidden());
        // Mouse moved, focus lost, left the video: once.
        assert_eq!(lead.update(Visible), LeadCursorChange::Restore);
        assert_eq!(lead.update(Visible), LeadCursorChange::Keep);
        assert!(!lead.is_hidden());
    }

    /// Pointer lock (F9 with the Mac mouse still), host cursor mode, and a
    /// host that hides its pointer when the Mac mouse moves all leave the
    /// viewer wanting no cursor: no arrow may come back.
    #[test]
    fn a_viewer_that_wants_no_cursor_never_gets_the_arrow_back() {
        use LeadCursorWant::{HostLeads, Invisible, Visible};
        for case in ["pointer lock", "host cursor mode", "host hides its pointer"] {
            let mut lead = LeadCursor::default();
            assert_eq!(lead.update(HostLeads), LeadCursorChange::Hide, "{case}");
            assert_eq!(lead.update(Invisible), LeadCursorChange::Hide, "{case}");
            assert_eq!(lead.update(Invisible), LeadCursorChange::Hide, "{case}");
            assert!(lead.is_hidden(), "{case}");
            // Released later over a viewer that shows a cursor: once.
            assert_eq!(lead.update(Visible), LeadCursorChange::Restore, "{case}");
        }
        // A viewer that never hid leaves winit's own hiding alone.
        let mut lead = LeadCursor::default();
        assert_eq!(lead.update(Invisible), LeadCursorChange::Keep);
        assert!(!lead.is_hidden());
    }

    #[test]
    fn a_retina_cursor_keeps_its_point_size_and_hotspot() {
        let image =
            CursorImage::from_straight_rgba(32, 32, (16, 8), [255, 0, 0, 255].repeat(32 * 32))
                .unwrap();
        // Shown at half a point per host pixel on a 2x display: 16 points,
        // drawn from 32 device pixels.
        let presented = image.present(0.5 * 2.0);
        let sharp = SharpCursor::new(&presented, 2.0).expect("AppKit cursor");
        let cursor_image = sharp.cursor.image();
        let size = cursor_image.size();
        assert_eq!((size.width, size.height), (16.0, 16.0));
        let hotspot = sharp.cursor.hotSpot();
        assert_eq!((hotspot.x, hotspot.y), (8.0, 4.0));
        // At 1x winit's cursor is already exact.
        assert!(SharpCursor::new(&image.present(1.0), 1.0).is_none());
        // A bitmap that disagrees with its size is refused.
        let mut broken = presented;
        broken.width += 1;
        assert!(SharpCursor::new(&broken, 2.0).is_none());
    }

    #[test]
    fn a_cursor_the_cap_would_shrink_keeps_winits_cursor() {
        let image =
            CursorImage::from_straight_rgba(256, 160, (10, 10), [0, 0, 255, 255].repeat(256 * 160))
                .unwrap();
        // 1.33 points per pixel on a 2x display: 683 device pixels wide.
        assert!(SharpCursor::for_image(&image, 1.33, 2.0).is_none());
        // 0.5 on 2x: 256 device pixels, drawn sharp at 128 points.
        let sharp = SharpCursor::for_image(&image, 0.5, 2.0).expect("sharp cursor");
        let size = sharp.cursor.image().size();
        assert_eq!((size.width, size.height), (128.0, 80.0));
    }
}
