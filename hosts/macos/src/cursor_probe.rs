// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What this host can say about the desktop's pointer, and the watcher that
//! says it to the Deck.
//!
//! `NSCursor.currentSystemCursor` reports the cursor the desktop is showing,
//! whichever process set it: its image and its hotspot. Measured with
//! [`probe`] on macOS 26.7 (ADR 0029 amendment 1): a sweep across a terminal
//! window and its edges read the I-beam, the arrow and the edge-resize
//! cursors as distinct images, the same with and without a pumped main run
//! loop, and the same from a background thread while the main thread slept,
//! which is how the agent reads it. Left where it was, over the terminal, it
//! read the I-beam.
//!
//! An earlier version of this module concluded the opposite ("the arrow
//! whatever the desktop is showing") and the Pier drew the pointer into the
//! video because of it. The reading was right; the naming was wrong. Shapes
//! were named by comparing the image's bytes with `NSCursor`'s class cursors,
//! and the window server's copy of the I-beam (9x18 points, two
//! representations) or of an edge-resize cursor never byte-matches `AppKit`'s
//! (the I-beam there is 23x22 points, four representations). Every unmatched
//! image was reported as the default arrow — the counters the watcher keeps
//! for exactly this said so, distinct images climbing while named shapes
//! did not — so every cursor looked like the arrow.
//!
//! So the host sends the image itself, as the shared `CursorUpdate` record
//! (`arcen_protocol::cursor`, the record the Linux and Windows Piers send),
//! and the Deck draws the pointer locally with the desktop's shape: it moves
//! at the speed of the hand instead of the video's. The image keeps its last
//! value while an application hides the pointer, so visibility is read
//! separately ([`desktop_cursor_visible`]), and each image is drawn at the
//! scale of the session display the pointer is on. A Deck that cannot draw
//! images is told the named shape where the image is one of the standard
//! cursors. Whether this process can read the cursor at all is checked live
//! when a session starts ([`desktop_cursor_images_readable`]); a host that
//! cannot keeps the pointer in the video (`showsCursor`).
//!
//! `currentSystemCursor` is deprecated, with Apple pointing capture at
//! `SCStreamConfiguration.showsCursor`. Deprecated here means superseded for
//! the common case, not removed; the probe stays so the claim can be
//! re-checked on a future macOS rather than taken on trust.

use std::time::Duration;

use arcen_protocol::messages::CursorShapeKind;
use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{ClassType as _, msg_send, sel};
// `currentSystemCursor` and some of the resize cursors carry deprecation
// warnings. Apple's replacement advice for the first is to composite the
// cursor into the capture with `SCStreamConfiguration.showsCursor`, which is a
// different design: it ties pointer movement to the video's latency, where
// this keeps the pointer local and only its image remote. The Deck renders
// these as native compositor cursors precisely so movement is not repaint
// gated. Deprecated here means superseded for the common case, not removed.
#[allow(deprecated)]
use objc2_app_kit::NSCursor;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSCursorFrameResizeDirections,
    NSCursorFrameResizePosition, NSImage,
};

/// Connects this process to the window server, without appearing anywhere.
///
/// [`NSCursor`]'s standard-cursor accessors return NULL until an `NSApplication`
/// exists, and the generated bindings would turn that NULL into a panic, which
/// is why `ShapeCatalogue` reads them through `class_cursor`. Measured on
/// the lab Mac: a session logged `cannot read the desktop cursor shape` because
/// every accessor came back NULL, even running as a proper Aqua `LaunchAgent`.
///
/// The activation policy is what makes this safe for a background agent.
/// `Prohibited` is the programmatic equivalent of `LSUIElement`: no Dock icon,
/// no menu bar, no window. The application is never run, so this brings up
/// no event loop of its own and does not compete with the runtime that serves
/// the session. Running one is not what reading the cursor needs: measured
/// with `probe-cursor --pump`, a pumped main run loop reads exactly the same
/// images as a sleeping one (ADR 0029).
///
/// Must be called on the main thread, before that thread is given to anything
/// else, which is why it is a startup step rather than something the watcher
/// does for itself. See [`CursorWatcher::start`].
///
/// Returns whether the cursor can be read afterwards, which is the only
/// question this connection exists to answer.
#[must_use]
#[allow(deprecated)]
pub fn connect_to_window_server() -> bool {
    let Some(marker) = objc2_foundation::MainThreadMarker::new() else {
        return false;
    };
    let application = NSApplication::sharedApplication(marker);
    // Set first, so the process never exists as a regular application even
    // briefly. Accessory would also avoid a Dock icon but still permits a menu
    // bar and windows; this host wants neither and should not be able to
    // acquire them by accident.
    //
    // The result is deliberately not what this function reports. Measured: a
    // process that reads all eight standard cursors happily can still have
    // this return false, because it answers "was the policy changed", and a
    // process already at the right policy changed nothing. Reporting that as a
    // failed connection produced a warning about a thing that was working.
    // All four activation policies were measured and none changed the answer,
    // so this is chosen for what it guarantees rather than for what it might
    // unlock: Prohibited is the programmatic equivalent of LSUIElement, and a
    // background agent must not be able to acquire a Dock icon by accident.
    let _ = application.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    // The real question, and the only one worth reporting: can the cursor be
    // read now. That is what the connection was for.
    NSCursor::currentSystemCursor().is_some()
}

/// How often the desktop is asked what cursor it is showing.
///
/// Sixty milliseconds is faster than a person can notice a pointer changing
/// shape and slow enough that the poll does not register against a session's
/// cost. The Deck is only told when the answer changes.
const POLL_INTERVAL: Duration = Duration::from_millis(60);

/// The standard cursors, by the bytes of their image.
///
/// Captured once. `currentSystemCursor` is not documented to return the same
/// object as `NSCursor::IBeamCursor` and friends, so identity cannot be used;
/// the image behind it is the thing that is actually the same.
#[derive(Debug)]
#[allow(deprecated)]
struct ShapeCatalogue {
    known: Vec<(Vec<u8>, CursorShapeKind)>,
}

impl ShapeCatalogue {
    #[allow(deprecated)]
    fn capture() -> Option<Self> {
        // Asked first: a process with no Aqua session, or a test binary
        // without AppKit, has no cursor to watch and should say so.
        NSCursor::currentSystemCursor()?;
        // The standard cursors do come back NULL in a process that has not
        // brought up AppKit, and this host must not bring it up: it runs as a
        // background agent with no Dock presence, and an NSApplication would
        // give it one. A host that cannot name the shapes reports none rather
        // than panicking.
        Self::from_images(standard_cursors().into_iter().map(|(cursor, kind)| {
            let image = match cursor {
                StandardCursor::Absent => StandardCursor::Absent,
                StandardCursor::Null => StandardCursor::Null,
                StandardCursor::Read(cursor) => match cursor_image(&cursor) {
                    // A cursor without an image is as unreadable as no cursor.
                    None => StandardCursor::Null,
                    Some(image) => {
                        tiff_bytes(&image).map_or(StandardCursor::Absent, StandardCursor::Read)
                    }
                },
            };
            (image, kind)
        }))
    }

    /// Builds the catalogue from the standard cursors' images, in naming
    /// order.
    fn from_images(
        images: impl IntoIterator<Item = (StandardCursor<Vec<u8>>, CursorShapeKind)>,
    ) -> Option<Self> {
        let mut known: Vec<(Vec<u8>, CursorShapeKind)> = Vec::new();
        for (image, kind) in images {
            let bytes = match image {
                StandardCursor::Absent => continue,
                // NULL from an accessor this macOS has means the process has
                // no cursor resources, and nothing else it reads can be
                // trusted to name a shape. Measured in a test binary: the
                // arrow and the I-beam NULL, and the pointing hand, open hand
                // and closed hand one shared placeholder image.
                StandardCursor::Null => return None,
                StandardCursor::Read(bytes) => bytes,
            };
            // First spelling wins: several cursors share an image, and the
            // earlier entry is the one whose name reads better in a log.
            if !known.iter().any(|(seen, _)| seen == &bytes) {
                known.push((bytes, kind));
            }
        }
        // A catalogue that recognises only the arrow would report every cursor
        // as an arrow, which is exactly the behaviour this replaces.
        if known.len() < 2 {
            return None;
        }
        Some(Self { known })
    }

    /// Names a cursor by the bytes of its image: the standard cursor it is,
    /// or the default arrow when it is none of them. The window server's
    /// copies of the I-beam and the resize cursors are not byte-identical to
    /// `AppKit`'s, so they read as the default here; that is why the image
    /// itself is what this host sends, and the name only a fallback.
    fn name(&self, bytes: &[u8]) -> CursorShapeKind {
        self.known
            .iter()
            .find(|(known, _)| known == bytes)
            .map_or(CursorShapeKind::Default, |(_, kind)| *kind)
    }
}

/// One standard cursor, as this process could read it.
#[derive(Debug, PartialEq, Eq)]
enum StandardCursor<T> {
    /// Left out: the running macOS predates it (the zoom, column and
    /// window-frame cursors arrived in macOS 15, and this Pier supports 14.2),
    /// or its image has no TIFF form to compare.
    Absent,
    /// `AppKit` has the accessor and answered NULL.
    Null,
    Read(T),
}

/// The standard cursors, in naming order.
fn standard_cursors() -> Vec<(StandardCursor<Retained<NSCursor>>, CursorShapeKind)> {
    let named = [
        (sel!(arrowCursor), CursorShapeKind::Default),
        (sel!(IBeamCursor), CursorShapeKind::Text),
        (sel!(pointingHandCursor), CursorShapeKind::Pointer),
        (sel!(crosshairCursor), CursorShapeKind::Crosshair),
        (sel!(openHandCursor), CursorShapeKind::Grab),
        (sel!(closedHandCursor), CursorShapeKind::Grabbing),
        (sel!(operationNotAllowedCursor), CursorShapeKind::NotAllowed),
        (sel!(zoomInCursor), CursorShapeKind::ZoomIn),
        (sel!(zoomOutCursor), CursorShapeKind::ZoomOut),
        // Edges in both spellings. Both are in circulation: the singletons
        // still appear inside views, the frame cursors on window chrome.
        (sel!(resizeUpDownCursor), CursorShapeKind::ResizeNs),
        (sel!(resizeLeftRightCursor), CursorShapeKind::ResizeEw),
        (sel!(rowResizeCursor), CursorShapeKind::ResizeNs),
        (sel!(columnResizeCursor), CursorShapeKind::ResizeEw),
    ];
    // Window frames resize from any edge on modern macOS, and the cursors for
    // that come from `frameResizeCursorFromPosition:inDirections:` rather than
    // from the old resize singletons. Leaving them out is what made a corner
    // drag show a plain arrow while the side edges worked: measured as 86
    // distinct cursor images against 59 that could be named.
    let frames = [
        (NSCursorFrameResizePosition::Top, CursorShapeKind::ResizeNs),
        (
            NSCursorFrameResizePosition::Bottom,
            CursorShapeKind::ResizeNs,
        ),
        (NSCursorFrameResizePosition::Left, CursorShapeKind::ResizeEw),
        (
            NSCursorFrameResizePosition::Right,
            CursorShapeKind::ResizeEw,
        ),
        // The corners, which is what "I did not get the size" was about.
        (
            NSCursorFrameResizePosition::TopLeft,
            CursorShapeKind::ResizeNwse,
        ),
        (
            NSCursorFrameResizePosition::BottomRight,
            CursorShapeKind::ResizeNwse,
        ),
        (
            NSCursorFrameResizePosition::TopRight,
            CursorShapeKind::ResizeNesw,
        ),
        (
            NSCursorFrameResizePosition::BottomLeft,
            CursorShapeKind::ResizeNesw,
        ),
    ];
    named
        .into_iter()
        .map(|(selector, kind)| (class_cursor(selector), kind))
        .chain(
            frames
                .into_iter()
                .map(|(position, kind)| (frame_resize_cursor(position), kind)),
        )
        .collect()
}

/// Whether `NSCursor` itself answers `selector` on this macOS.
fn cursor_class_responds_to(selector: Sel) -> bool {
    NSCursor::class().metaclass().responds_to(selector)
}

/// Reads one of `NSCursor`'s argument-less class cursors.
///
/// Not through the generated bindings, which declare these accessors
/// non-null and panic on the NULL that `AppKit` returns in a process without an
/// `NSApplication` (every test binary, measured: `+[NSCursor arrowCursor]`).
/// And an accessor newer than the running macOS is an unrecognised selector:
/// an Objective-C exception, which Rust cannot catch. So the class is asked
/// first.
#[allow(unsafe_code)]
fn class_cursor(selector: Sel) -> StandardCursor<Retained<NSCursor>> {
    if !cursor_class_responds_to(selector) {
        return StandardCursor::Absent;
    }
    // SAFETY: `NSCursor` responds to `selector`, a class method that takes no
    // arguments and returns an `NSCursor` or nil; `performSelector:` returns
    // that object unretained, and `Retained` takes its own reference.
    let cursor: Option<Retained<NSCursor>> =
        unsafe { msg_send![NSCursor::class(), performSelector: selector] };
    cursor.map_or(StandardCursor::Null, StandardCursor::Read)
}

/// Reads a window-frame resize cursor; see [`class_cursor`].
#[allow(unsafe_code)]
fn frame_resize_cursor(
    position: NSCursorFrameResizePosition,
) -> StandardCursor<Retained<NSCursor>> {
    if !cursor_class_responds_to(sel!(frameResizeCursorFromPosition:inDirections:)) {
        return StandardCursor::Absent;
    }
    // SAFETY: `NSCursor` responds to the selector, which takes a frame
    // position and a direction mask and returns an `NSCursor` or nil.
    let cursor: Option<Retained<NSCursor>> = unsafe {
        msg_send![
            NSCursor::class(),
            frameResizeCursorFromPosition: position,
            inDirections: NSCursorFrameResizeDirections::All
        ]
    };
    cursor.map_or(StandardCursor::Null, StandardCursor::Read)
}

/// Returns the bytes of a cursor's image, for comparison only.
fn image_bytes(cursor: &NSCursor) -> Option<Vec<u8>> {
    cursor_image(cursor).and_then(|image| tiff_bytes(&image))
}

/// A cursor's image, read nullable for the same reason as `class_cursor`: the
/// binding for `image` is non-null, and the watcher calls this on its own
/// thread, where a panic would end the watch without a word.
#[allow(unsafe_code)]
fn cursor_image(cursor: &NSCursor) -> Option<Retained<NSImage>> {
    // SAFETY: `image` takes no arguments and returns an `NSImage` or nil.
    unsafe { msg_send![cursor, image] }
}

/// An image's TIFF serialisation, which borrows nothing from the image.
fn tiff_bytes(image: &NSImage) -> Option<Vec<u8>> {
    Some(image.TIFFRepresentation()?.to_vec())
}

/// Where the probe's sweep should put the pointer on a given step.
///
/// A lap around the middle of the desktop rather than a straight line: a
/// straight line can miss every text field and every window edge on a desktop
/// whose windows happen to sit elsewhere.
fn sweep_point(step: u32) -> (f64, f64) {
    let angle = f64::from(step) * 0.4;
    (700.0 + angle.cos() * 420.0, 420.0 + angle.sin() * 300.0)
}

#[allow(unsafe_code)]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// `boolean_t CGCursorIsVisible(void)`, `CGRemoteOperation.h`.
    fn CGCursorIsVisible() -> i32;
    /// `uint32_t CGEventSourceCounterForEventType(CGEventSourceStateID,
    /// CGEventType)`, `CGEventSource.h`.
    fn CGEventSourceCounterForEventType(state: i32, event_type: u32) -> u32;
    /// `bool CGEventSourceButtonState(CGEventSourceStateID, CGMouseButton)`,
    /// `CGEventSource.h`.
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
}

/// `kCGEventSourceStateCombinedSessionState`: hardware and posted events.
const COMBINED_SESSION_STATE: i32 = 0;
/// `kCGEventKeyDown`.
const KEY_DOWN: u32 = 10;

/// Whether the desktop shows its pointer now.
///
/// `currentSystemCursor` keeps answering the last image while the pointer is
/// hidden, so visibility is read separately. Measured from a background
/// process on macOS 26.7 (ADR 0029 amendment 1): `CGCursorIsVisible` turns
/// false while another application hides the pointer with `NSCursor.hide`,
/// with `setHiddenUntilMouseMoves:` (what a text view does on typing — also
/// measured by typing into one) and with `CGDisplayHideCursor`, and true
/// again when it shows it. Deprecated without a replacement, like
/// `currentSystemCursor`; `probe-cursor` re-checks both.
#[allow(unsafe_code)]
#[must_use]
pub fn desktop_cursor_visible() -> bool {
    // SAFETY: takes no arguments and only reads the window server's cursor
    // state.
    unsafe { CGCursorIsVisible() != 0 }
}

/// Key-downs the session has seen so far, posted ones included: a key typed
/// may hide the pointer until it moves again.
#[allow(unsafe_code)]
fn key_downs_so_far() -> u32 {
    // SAFETY: plain values in, a counter out; only reads event-source state.
    unsafe { CGEventSourceCounterForEventType(COMBINED_SESSION_STATE, KEY_DOWN) }
}

/// Whether the left, right or middle button is held, posted presses
/// included.
#[allow(unsafe_code)]
fn mouse_button_held() -> bool {
    // SAFETY: plain values in, a flag out; only reads event-source state.
    (0..3).any(|button| unsafe { CGEventSourceButtonState(COMBINED_SESSION_STATE, button) })
}

/// Shows the pointer again once it moves while the desktop hides it.
///
/// An application that hides the pointer "until the mouse moves"
/// (`setHiddenUntilMouseMoves:`, what Chrome does on every key typed into a
/// web text field, `render_widget_host_view_cocoa.mm`) is shown again by the
/// window server only when the hardware moves. The motion this host posts
/// does not count. Measured on macOS 26 with a window that hid the pointer
/// that way: a warp plus a posted move at the HID tap (the session's
/// motion), a move alone, either with its delta fields set, at the session
/// tap or from any event source, a zero scroll, and `unhide` or
/// `CGDisplayShowCursor` from another process all left `CGCursorIsVisible`
/// false while the window received every move. Only a posted click showed
/// it again. So a Deck user who typed a search and then moved the mouse saw
/// no pointer until they clicked or the image changed.
///
/// The pointer is therefore reported shown once its position changes while
/// hidden with no mouse button held, as the hardware would have shown it,
/// until the desktop shows it itself. A key typed while it is hidden starts
/// over: the window server still hides it, and the key may have hidden it
/// again. Movement with a button held does not count, so an application that
/// hides the pointer for a drag (a scrub, a knob) keeps it hidden. Nothing
/// public tells a typing hide from `NSCursor.hide`; an application that
/// hides the pointer that way and keeps it hidden while it moves with no
/// button held (pointer lock, which the session's warps already undo) shows
/// the Deck's pointer while it moves.
#[derive(Debug, Default)]
struct HiddenUntilMoved {
    /// Where the pointer was when it was last seen hidden and still.
    hidden_at: Option<(f64, f64)>,
    moved: bool,
    /// The key-down count at the previous reading.
    key_downs: Option<u32>,
}

impl HiddenUntilMoved {
    /// Whether to report the pointer shown, given whether the window server
    /// shows it, where the pointer is now, the session's key-down count and
    /// whether a mouse button is held.
    fn shown(
        &mut self,
        visible: bool,
        pointer: Option<(f64, f64)>,
        key_downs: u32,
        button_held: bool,
    ) -> bool {
        let typed = self.key_downs.is_some_and(|seen| seen != key_downs);
        self.key_downs = Some(key_downs);
        if visible {
            self.hidden_at = None;
            self.moved = false;
            return true;
        }
        if typed {
            self.moved = false;
            self.hidden_at = pointer;
        }
        if !self.moved {
            match (self.hidden_at, pointer) {
                (_, Some(now)) if button_held => self.hidden_at = Some(now),
                (None, Some(at)) => self.hidden_at = Some(at),
                (Some(at), Some(now)) if at != now => self.moved = true,
                _ => {}
            }
        }
        self.moved
    }
}

/// The bitmap a cursor `width` x `height` points is drawn into at
/// `pixels_per_point`: its size in pixels and the scale actually used, which
/// is smaller than asked when the image would not fit the largest cursor a
/// host may send (a magnified pointer). `None` for an empty image.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn bitmap_geometry(width: f64, height: f64, pixels_per_point: f64) -> Option<(u32, u32, f64)> {
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return None;
    }
    let largest = f64::from(arcen_media::cursor::MAX_SOURCE_CURSOR_SIDE);
    let wanted = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let scale = wanted.min(largest / width.max(height));
    let side = |points: f64| (points * scale).round().clamp(1.0, largest) as u32;
    Some((side(width), side(height), scale))
}

/// A hotspot in points, in the pixels of a bitmap drawn at `scale`.
#[allow(clippy::cast_possible_truncation)]
fn hotspot_pixels(x: f64, y: f64, scale: f64) -> (i64, i64) {
    let pixel = |points: f64| {
        let value = (points * scale).floor();
        if value.is_finite() { value as i64 } else { 0 }
    };
    (pixel(x), pixel(y))
}

/// A cursor's image as straight-alpha RGBA in stream pixels, with its
/// hotspot: what the shared `CursorUpdate` record carries.
///
/// `NSCursor.hotSpot` is in the image's own top-left-origin points, and a
/// bitmap context stores its top row first, so both come out the way the
/// record wants them. Read off the main thread by the watcher, as
/// [`image_bytes`] always has been: nothing here touches a window.
#[allow(unsafe_code)]
fn cursor_bitmap(
    cursor: &NSCursor,
    pixels_per_point: f64,
) -> Option<arcen_media::cursor::CursorImage> {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, kCGColorSpaceSRGB,
    };

    let image = cursor_image(cursor)?;
    let size = image.size();
    let (width, height, scale) = bitmap_geometry(size.width, size.height, pixels_per_point)?;
    let pixel_rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(f64::from(width), f64::from(height)),
    );
    let mut proposed = pixel_rect;
    // SAFETY: `proposed` is a valid rectangle that outlives the call; no
    // reference context and no hints are allowed by the API. The result is a
    // retained image or nil.
    let picture =
        unsafe { image.CGImageForProposedRect_context_hints(&raw mut proposed, None, None) }?;
    // SAFETY: an immutable framework constant, valid for the process.
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let columns = usize::try_from(width).ok()?;
    let rows = usize::try_from(height).ok()?;
    let row_bytes = columns.checked_mul(4)?;
    let mut pixels = vec![0_u8; row_bytes.checked_mul(rows)?];
    // SAFETY: `pixels` holds `row_bytes * rows` bytes and is neither moved nor
    // read until `context`, the only writer, has been dropped below. Eight
    // bits per component, premultiplied RGBA in sRGB, is a format bitmap
    // contexts support.
    let context = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            columns,
            rows,
            8,
            row_bytes,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }?;
    CGContext::draw_image(Some(&context), pixel_rect, Some(&picture));
    drop(context);
    let hotspot = cursor.hotSpot();
    arcen_media::cursor::CursorImage::from_premultiplied_rgba(
        width,
        height,
        hotspot_pixels(hotspot.x, hotspot.y, scale),
        &pixels,
    )
    .ok()
}

/// The image of the cursor the desktop is showing now, in stream pixels at
/// `pixels_per_point`.
#[must_use]
#[allow(deprecated)]
pub fn current_cursor_image(pixels_per_point: f64) -> Option<arcen_media::cursor::CursorImage> {
    objc2::rc::autoreleasepool(|_| {
        NSCursor::currentSystemCursor().and_then(|cursor| cursor_bitmap(&cursor, pixels_per_point))
    })
}

/// Whether this process can read the desktop cursor's image now: the live
/// check a session makes before it tells a Deck the host sends cursor images.
/// False in a process without a window-server connection, which then keeps
/// the pointer in the video.
#[must_use]
pub fn desktop_cursor_images_readable() -> bool {
    current_cursor_image(1.0).is_some_and(|image| !image.is_fully_transparent())
}

/// How [`probe`] samples.
#[derive(Debug, Clone, Copy)]
pub struct ProbeOptions {
    /// How long to sample for.
    pub sample_for: Duration,
    /// Move the pointer in a lap over the desktop while sampling, and put it
    /// back afterwards. Without it the probe only watches.
    pub sweep: bool,
    /// Run the main thread's run loop between samples instead of sleeping:
    /// the experiment for whether `AppKit` needs window-server notifications
    /// delivered to read the cursor (measured: it does not).
    pub pump_run_loop: bool,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            sample_for: Duration::from_secs(6),
            sweep: true,
            pump_run_loop: false,
        }
    }
}

/// What this machine can tell a Deck about its cursor.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, serde::Serialize)]
pub struct CursorProbe {
    /// Whether a cursor could be read after connecting to the window server.
    pub window_server: bool,
    /// Whether the desktop answered with a cursor at all.
    pub current_cursor_readable: bool,
    /// Whether the cursor's image could be drawn into a bitmap: what a
    /// session checks before it offers cursor images.
    pub image_readable: bool,
    /// Whether the pointer was swept (and put back) while sampling.
    pub swept: bool,
    /// Whether the main run loop ran between samples.
    pub pumped: bool,
    /// Samples taken.
    pub samples: u32,
    /// How many standard shapes were identified by image.
    pub shapes_known: usize,
    /// Distinct cursor *images* seen, by digest.
    ///
    /// Reported separately from the named shapes because the two answer
    /// different questions, and conflating them produced a wrong conclusion
    /// once already: an image that matches no known cursor is reported as the
    /// default arrow, which is indistinguishable from the cursor genuinely
    /// being an arrow. More than one here means the cursor can be followed,
    /// whatever `shapes_seen` says.
    pub distinct_images: usize,
    /// Each distinct image as it would be sent: size in pixels at one pixel
    /// per point, hotspot, and the standard shape it matched, in order seen.
    pub images: Vec<String>,
    /// Every distinct named shape seen while sampling.
    pub shapes_seen: Vec<String>,
    /// Samples in which the desktop showed its pointer, and in which it hid
    /// it (`CGCursorIsVisible`): an application hiding the pointer while
    /// the probe runs shows up as hidden samples.
    pub visible_samples: u32,
    /// See [`Self::visible_samples`].
    pub hidden_samples: u32,
    /// What the session's watcher sent while the probe ran, decoded: the
    /// real `CursorUpdate` records for a Deck that draws images, over every
    /// active display at its own scale, in order (repeats folded).
    pub watcher_records: Vec<String>,
}

#[allow(unsafe_code)]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: *const std::ffi::c_void;
    fn CFRunLoopRunInMode(mode: *const std::ffi::c_void, seconds: f64, return_after: u8) -> i32;
}

/// Waits one poll interval: on the run loop when asked to pump it.
#[allow(unsafe_code)]
fn wait_one_poll(pump_run_loop: bool) {
    if pump_run_loop {
        // SAFETY: runs this thread's run loop in the default mode for at most
        // one poll interval; the mode constant is valid for the process.
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, POLL_INTERVAL.as_secs_f64(), 0);
        }
    } else {
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Reports whether this host could follow the desktop's pointer.
///
/// With [`ProbeOptions::sweep`] the pointer is moved while sampling, so the
/// answer covers the question that matters — whether the cursor is seen to
/// *change* — rather than only whether one can be read at all, and is put
/// back where it was afterwards. Without it, the probe only watches.
///
/// Must run on the main thread, because connecting to the window server does.
#[must_use]
#[allow(deprecated)]
pub fn probe(options: ProbeOptions) -> CursorProbe {
    let window_server = connect_to_window_server();
    let current_cursor_readable = NSCursor::currentSystemCursor().is_some();
    let image_readable = desktop_cursor_images_readable();
    let catalogue = ShapeCatalogue::capture();
    let mut seen: Vec<String> = Vec::new();
    let mut digests: Vec<(usize, u64)> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    let mut samples = 0_u32;
    let (mut visible_samples, mut hidden_samples) = (0_u32, 0_u32);
    let start = if options.sweep {
        crate::input::current_pointer_position()
    } else {
        None
    };
    // Never sweep a pointer that could not be put back.
    let sweep = options.sweep && start.is_some();
    let watcher = CursorWatcher::start(CursorReport::Images {
        outputs: every_display(),
    });
    let mut watcher_records: Vec<String> = Vec::new();
    if current_cursor_readable {
        let deadline = std::time::Instant::now() + options.sample_for;
        while std::time::Instant::now() < deadline {
            if sweep {
                crate::input::move_pointer_for_probe(sweep_point(samples));
            }
            wait_one_poll(options.pump_run_loop);
            samples = samples.saturating_add(1);
            if let Some(CursorMessage::Record(record)) =
                watcher.as_ref().and_then(CursorWatcher::take_changed)
            {
                let described = describe_record(&record);
                if watcher_records.last() != Some(&described) {
                    watcher_records.push(described);
                }
            }
            if desktop_cursor_visible() {
                visible_samples = visible_samples.saturating_add(1);
            } else {
                hidden_samples = hidden_samples.saturating_add(1);
            }
            objc2::rc::autoreleasepool(|_| {
                let Some(cursor) = NSCursor::currentSystemCursor() else {
                    return;
                };
                let Some(bytes) = image_bytes(&cursor) else {
                    return;
                };
                let shape = catalogue
                    .as_ref()
                    .map_or(CursorShapeKind::Default, |catalogue| catalogue.name(&bytes));
                let shape = format!("{shape:?}");
                let digest = bytes_digest(&bytes, cursor.hotSpot());
                if !digests.contains(&digest) {
                    digests.push(digest);
                    images.push(cursor_bitmap(&cursor, 1.0).map_or_else(
                        || format!("unreadable bitmap ({shape})"),
                        |image| {
                            let (x, y) = image.hotspot();
                            format!(
                                "{}x{} hotspot {x},{y} ({shape})",
                                image.width(),
                                image.height()
                            )
                        },
                    ));
                }
                if !seen.contains(&shape) {
                    seen.push(shape);
                }
            });
        }
    }
    if let Some(position) = start.filter(|_| sweep) {
        crate::input::warp_pointer(position);
    }
    CursorProbe {
        window_server,
        current_cursor_readable,
        image_readable,
        swept: sweep,
        pumped: options.pump_run_loop,
        samples,
        shapes_known: catalogue.map_or(0, |c| c.known.len()),
        distinct_images: digests.len(),
        images,
        shapes_seen: seen,
        visible_samples,
        hidden_samples,
        watcher_records,
    }
}

/// Every active display, numbered by its display id, at its own pixels per
/// point: what a Match My Layout session over all of them would use.
fn every_display() -> Vec<arcen_input::cursor::PointerOutput> {
    crate::displays::probe()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|display| {
            let bounds = crate::displays::point_bounds(display.display_id)?;
            Some(arcen_input::cursor::PointerOutput::new(
                display.display_id,
                0,
                bounds,
                u32::try_from(display.pixel_width).ok()?,
            ))
        })
        .collect()
}

/// One watcher record as the probe reports it.
fn describe_record(record: &[u8]) -> String {
    use arcen_protocol::cursor::CursorAppearance;
    match arcen_protocol::cursor::decode_cursor_update(record) {
        Ok(update) => match update.appearance {
            CursorAppearance::Hidden => format!("hidden on output {}", update.output),
            CursorAppearance::Image(image) => format!(
                "image {} {}x{} hotspot {},{} on output {}",
                image.id.get(),
                image.width,
                image.height,
                image.hotspot_x,
                image.hotspot_y,
                update.output
            ),
        },
        Err(error) => format!("undecodable: {error}"),
    }
}

/// A cheap digest of a cursor's image bytes and hotspot, for telling cursors
/// apart. Length plus a rolling sum is enough; this exists to notice a
/// change, not to identify an image.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn bytes_digest(bytes: &[u8], hotspot: objc2_foundation::NSPoint) -> (usize, u64) {
    let sum = bytes.iter().fold(0_u64, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(u64::from(*byte))
    });
    let spot = (hotspot.x.max(0.0) as u64) << 16 | (hotspot.y.max(0.0) as u64);
    (bytes.len(), sum ^ spot.rotate_left(48))
}

/// What a session should tell the Deck about the desktop's cursor.
#[derive(Debug, Clone, PartialEq)]
pub enum CursorReport {
    /// The cursor's name (`cursor_shape`), for a Deck that draws no images.
    Shapes,
    /// The cursor's image (`CursorUpdate`), drawn at the scale of the
    /// session's output the pointer is on and tagged with that output.
    Images {
        /// The session's outputs, in desktop points: one with output 0 for
        /// a single display, one per display (numbered by its
        /// `CGDirectDisplayID`) for Match My Layout.
        outputs: Vec<arcen_input::cursor::PointerOutput>,
    },
}

/// One message for the Deck from the watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorMessage {
    /// A `cursor_shape` message.
    Shape(arcen_protocol::messages::CursorShapeMsg),
    /// A `CursorUpdate` record with its image inline, for the lane relay
    /// (`arcen_protocol::cursor::CursorRecordRelay`), which inlines it only
    /// while the Deck does not hold that image.
    Record(Vec<u8>),
}

/// The newest change the watcher has not handed over yet.
#[derive(Debug)]
enum Pending {
    Shape(CursorShapeKind),
    Record(Vec<u8>),
}

/// Watches the desktop cursor and reports its changes.
///
/// Polls, because nothing notifies. It reads `currentSystemCursor`, its
/// `image`, and its `hotSpot`.
///
/// With [`CursorReport::Images`] a hidden pointer, and every new image drawn
/// at the scale of the output under the pointer, go through the shared
/// [`arcen_input::cursor::HostCursor`] (stable ids, repeats suppressed, new
/// images rate-limited) in that output's pixel space, as on the Windows
/// Pier. With
/// [`CursorReport::Shapes`] the image is named against the standard cursors,
/// and one that matches none is reported as the default arrow.
#[derive(Debug)]
pub struct CursorWatcher {
    // Only the newest change matters: a pointer's shape is state, and both
    // a newer shape and a newer whole-state record supersede an older one.
    pending: std::sync::Arc<std::sync::Mutex<Option<Pending>>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Distinct cursor images seen, whether or not they were named.
    ///
    /// Separated from what was sent because conflating them produced a wrong
    /// conclusion once: an unmatched image reads as the default arrow,
    /// which is indistinguishable from the cursor really being an arrow. If
    /// this climbs while the Deck never sees a change, the polling works and
    /// what follows it is what is broken.
    images_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Images that could not be drawn into a bitmap or fitted to the wire.
    images_refused: std::sync::Arc<std::sync::atomic::AtomicU64>,
    sequence: std::sync::atomic::AtomicU64,
}

impl CursorWatcher {
    /// Starts watching, or returns `None` when the cursor cannot be read
    /// the way `report` needs it.
    #[must_use]
    pub fn start(report: CursorReport) -> Option<Self> {
        let catalogue = ShapeCatalogue::capture();
        match report {
            // Naming needs the catalogue; sending images does not.
            CursorReport::Shapes if catalogue.is_none() => return None,
            CursorReport::Images { ref outputs }
                if outputs.is_empty() || current_cursor_image(1.0).is_none() =>
            {
                return None;
            }
            CursorReport::Shapes | CursorReport::Images { .. } => {}
        }
        let pending = std::sync::Arc::new(std::sync::Mutex::new(None));
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let images_seen = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let images_refused = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut poll = WatcherPoll {
            report,
            catalogue,
            host: arcen_input::cursor::HostCursor::default(),
            record_sequence: 0,
            last_image: None,
            last_shown: None,
            output: None,
            last_shape: None,
            hidden_until_moved: HiddenUntilMoved::default(),
            pending: std::sync::Arc::clone(&pending),
            images_seen: std::sync::Arc::clone(&images_seen),
            images_refused: std::sync::Arc::clone(&images_refused),
        };
        let thread_cancelled = std::sync::Arc::clone(&cancelled);
        std::thread::Builder::new()
            .name("arcen-cursor".to_owned())
            .spawn(move || {
                while !thread_cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    // Each poll's AppKit objects are autoreleased. A plain
                    // thread has no pool that drains until it exits, so without
                    // one per iteration every cursor image read for the whole
                    // session stays allocated.
                    objc2::rc::autoreleasepool(|_| poll.once());
                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .ok()?;
        Some(Self {
            pending,
            cancelled,
            images_seen,
            images_refused,
            sequence: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Returns the newest change, if there has been one since the last call.
    ///
    /// Never blocks, and skips superseded changes: sending the shapes a
    /// pointer has already left costs the writer for nothing.
    pub fn take_changed(&self) -> Option<CursorMessage> {
        let pending = self.pending.lock().ok()?.take()?;
        Some(match pending {
            Pending::Record(record) => CursorMessage::Record(record),
            Pending::Shape(shape) => {
                CursorMessage::Shape(arcen_protocol::messages::CursorShapeMsg {
                    msg_type: arcen_protocol::messages::CURSOR_SHAPE.to_owned(),
                    shape,
                    // Starts at one: the Deck ignores any sequence not greater
                    // than the last applied, and zero is what an absent field
                    // deserialises to.
                    sequence: self
                        .sequence
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1,
                })
            }
        })
    }

    /// How many distinct cursor images this session has seen.
    #[must_use]
    pub fn images_seen(&self) -> u64 {
        self.images_seen.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many cursor images could not be sent: not drawable into a bitmap,
    /// or not fittable to the wire. The Deck keeps the previous cursor.
    #[must_use]
    pub fn images_refused(&self) -> u64 {
        self.images_refused
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for CursorWatcher {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The watcher thread's state between polls.
struct WatcherPoll {
    report: CursorReport,
    catalogue: Option<ShapeCatalogue>,
    host: arcen_input::cursor::HostCursor,
    record_sequence: u64,
    /// The digest of the last image read, for counting distinct images.
    last_image: Option<(usize, u64)>,
    /// What was last offered to `host`: hidden, or this image on the output
    /// at this index. An image is drawn again only when this changes.
    last_shown: Option<Shown>,
    /// Index into the report's outputs of the one the pointer is on.
    output: Option<usize>,
    last_shape: Option<CursorShapeKind>,
    hidden_until_moved: HiddenUntilMoved,
    pending: std::sync::Arc<std::sync::Mutex<Option<Pending>>>,
    images_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    images_refused: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// What the watcher last offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Hidden,
    Image { digest: (usize, u64), output: usize },
}

impl WatcherPoll {
    /// Reads the cursor once and records any change for the session.
    #[allow(deprecated)]
    fn once(&mut self) {
        use arcen_input::cursor::HostCursorOutcome;

        let now = std::time::Instant::now();
        let pointer = crate::input::current_pointer_position();
        let visible = self.hidden_until_moved.shown(
            desktop_cursor_visible(),
            pointer,
            key_downs_so_far(),
            mouse_button_held(),
        );
        let cursor = NSCursor::currentSystemCursor();
        let bytes = cursor.as_deref().and_then(image_bytes);
        let digest = cursor
            .as_deref()
            .zip(bytes.as_deref())
            .map(|(cursor, bytes)| bytes_digest(bytes, cursor.hotSpot()));
        if digest.is_some() && digest != self.last_image {
            self.last_image = digest;
            self.images_seen
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let shape = bytes.as_deref().map(|bytes| {
            self.catalogue
                .as_ref()
                .map_or(CursorShapeKind::Default, |catalogue| catalogue.name(bytes))
        });
        let CursorReport::Images { outputs } = &self.report else {
            let shape = if visible {
                shape
            } else {
                Some(CursorShapeKind::Hidden)
            };
            if let Some(shape) = shape.filter(|shape| self.last_shape != Some(*shape)) {
                self.last_shape = Some(shape);
                self.hand_over(Pending::Shape(shape));
            }
            return;
        };
        self.output = arcen_input::cursor::output_under_pointer(outputs, pointer, self.output);
        let shown = if visible {
            digest
                .zip(self.output)
                .map(|(digest, output)| Shown::Image { digest, output })
        } else {
            Some(Shown::Hidden)
        };
        let outcome = match shown {
            Some(next) if self.last_shown != Some(next) => {
                self.last_shown = Some(next);
                self.offer(next, cursor.as_deref(), shape.unwrap_or_default(), now)
            }
            _ if self.host.retry_at().is_some_and(|at| now >= at) => self.host.poll(now),
            _ => HostCursorOutcome::Unchanged,
        };
        match outcome {
            HostCursorOutcome::Changed => {
                let Some(facts) = self.host.current() else {
                    return;
                };
                self.record_sequence += 1;
                if let Ok(record) =
                    arcen_input::cursor::agent_cursor_record(facts, self.record_sequence)
                {
                    self.hand_over(Pending::Record(record));
                }
            }
            HostCursorOutcome::ImageRefused(_) => {
                self.images_refused
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            HostCursorOutcome::Deferred | HostCursorOutcome::Unchanged => {}
        }
    }

    /// Offers `shown` to the shared host cursor: hidden, or the image drawn
    /// at its output's scale, in that output's pixel space.
    fn offer(
        &mut self,
        shown: Shown,
        cursor: Option<&NSCursor>,
        fallback: CursorShapeKind,
        now: std::time::Instant,
    ) -> arcen_input::cursor::HostCursorOutcome {
        use arcen_input::cursor::{HostCursorOutcome, HostCursorSample};

        let Shown::Image { output, .. } = shown else {
            return self.host.offer(HostCursorSample::Hidden, now);
        };
        let CursorReport::Images { outputs } = &self.report else {
            return HostCursorOutcome::Unchanged;
        };
        let Some(target) = outputs.get(output) else {
            return HostCursorOutcome::Unchanged;
        };
        let Some(image) = cursor.and_then(|cursor| cursor_bitmap(cursor, target.pixels_per_unit))
        else {
            self.images_refused
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return HostCursorOutcome::Unchanged;
        };
        self.host.set_output(target.output, target.generation);
        self.host
            .offer(HostCursorSample::Image { image, fallback }, now)
    }

    fn hand_over(&self, change: Pending) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some(change);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HiddenUntilMoved, ShapeCatalogue, StandardCursor, class_cursor, image_bytes,
        standard_cursors, sweep_point,
    };
    use arcen_protocol::messages::CursorShapeKind;

    #[test]
    fn reading_the_standard_cursors_in_a_process_without_appkit_does_not_panic() {
        // A test binary has no NSApplication, and AppKit answers NULL for
        // `+[NSCursor arrowCursor]` here (measured). Read the way a session
        // reads them, with nothing to catch a panic.
        for (cursor, _) in standard_cursors() {
            if let StandardCursor::Read(cursor) = cursor {
                let _ = image_bytes(&cursor);
            }
        }
        let _ = ShapeCatalogue::capture();
    }

    #[test]
    fn a_cursor_this_macos_does_not_have_reads_as_none() {
        // Sent without asking first, this is an Objective-C exception.
        assert!(matches!(
            class_cursor(objc2::sel!(arcenCursorNoMacOSHas)),
            StandardCursor::Absent
        ));
    }

    #[test]
    fn a_cursor_this_macos_does_not_have_is_left_out_of_the_catalogue() {
        let catalogue = ShapeCatalogue::from_images([
            (StandardCursor::Read(vec![1]), CursorShapeKind::Text),
            (StandardCursor::Absent, CursorShapeKind::ZoomIn),
            (StandardCursor::Read(vec![2]), CursorShapeKind::Crosshair),
            (StandardCursor::Read(vec![1]), CursorShapeKind::Grab),
        ]);
        assert_eq!(
            catalogue.map(|catalogue| catalogue.known),
            Some(vec![
                (vec![1], CursorShapeKind::Text),
                (vec![2], CursorShapeKind::Crosshair),
            ])
        );
    }

    #[test]
    fn a_standard_cursor_answered_as_null_means_no_catalogue() {
        assert!(
            ShapeCatalogue::from_images([
                (StandardCursor::Null, CursorShapeKind::Default),
                (StandardCursor::Read(vec![1]), CursorShapeKind::Text),
                (StandardCursor::Read(vec![2]), CursorShapeKind::Pointer),
            ])
            .is_none()
        );
    }

    #[test]
    fn a_catalogue_that_can_name_one_shape_is_none() {
        assert!(
            ShapeCatalogue::from_images([
                (StandardCursor::Read(vec![1]), CursorShapeKind::Default),
                (StandardCursor::Absent, CursorShapeKind::ZoomIn),
                (StandardCursor::Read(vec![1]), CursorShapeKind::Pointer),
            ])
            .is_none()
        );
    }

    #[test]
    fn the_standard_cursors_are_distinguishable_from_each_other() {
        // If two standard cursors serialised to the same bytes, one could
        // never be reported even on a system where reporting worked at all.
        let Some(catalogue) = ShapeCatalogue::capture() else {
            eprintln!("no cursor catalogue in this process; nothing to check");
            return;
        };
        for (index, (bytes, kind)) in catalogue.known.iter().enumerate() {
            for (other, other_kind) in catalogue.known.iter().skip(index + 1) {
                assert_ne!(
                    bytes, other,
                    "{kind:?} and {other_kind:?} have identical images"
                );
            }
        }
    }

    #[test]
    fn an_unrecognised_cursor_reads_as_the_default() {
        // An application's own cursor matches nothing in the catalogue.
        let catalogue = ShapeCatalogue {
            known: vec![(vec![1], CursorShapeKind::Text)],
        };
        assert_eq!(catalogue.name(&[2]), CursorShapeKind::Default);
        assert_eq!(catalogue.name(&[1]), CursorShapeKind::Text);
    }

    #[test]
    fn the_sweep_stays_on_a_desktop() {
        // Nothing here should ask the window server to put the pointer at a
        // negative coordinate.
        for step in 0..64 {
            let (x, y) = sweep_point(step);
            assert!(x >= 0.0 && y >= 0.0, "step {step} sweeps to {x},{y}");
        }
    }

    #[test]
    fn a_pointer_hidden_until_it_moves_is_shown_once_it_moves() {
        // Chrome hides the pointer while you type a search; the session's
        // posted motion never makes the window server show it again.
        let mut hide = HiddenUntilMoved::default();
        assert!(hide.shown(true, Some((10.0, 10.0)), 0, false));
        assert!(!hide.shown(false, Some((10.0, 10.0)), 0, false));
        assert!(!hide.shown(false, Some((10.0, 10.0)), 0, false));
        assert!(hide.shown(false, Some((12.5, 10.0)), 0, false));
        assert!(hide.shown(false, Some((12.5, 10.0)), 0, false));
        // The desktop showing it again ends the hide; the next one waits for
        // a move again.
        assert!(hide.shown(true, Some((12.5, 10.0)), 0, false));
        assert!(!hide.shown(false, Some((12.5, 10.0)), 0, false));
        assert!(hide.shown(false, Some((40.0, 3.0)), 0, false));
    }

    #[test]
    fn a_key_typed_after_the_move_hides_the_pointer_until_it_moves_again() {
        // The window server never showed the pointer, so typing again (which
        // hides it again in Chrome) comes with no visible-to-hidden edge.
        let mut hide = HiddenUntilMoved::default();
        assert!(!hide.shown(false, Some((10.0, 10.0)), 7, false));
        assert!(hide.shown(false, Some((20.0, 10.0)), 7, false));
        assert!(!hide.shown(false, Some((20.0, 10.0)), 8, false));
        assert!(!hide.shown(false, Some((20.0, 10.0)), 8, false));
        assert!(hide.shown(false, Some((25.0, 12.0)), 8, false));
    }

    #[test]
    fn a_drag_while_hidden_keeps_the_pointer_hidden() {
        // An application that hides the pointer for a drag (a scrub, a knob)
        // keeps it hidden while the button is held.
        let mut hide = HiddenUntilMoved::default();
        assert!(!hide.shown(false, Some((10.0, 10.0)), 0, true));
        assert!(!hide.shown(false, Some((30.0, 10.0)), 0, true));
        assert!(!hide.shown(false, Some((60.0, 10.0)), 0, true));
        // Released where the drag ended: not a move of its own.
        assert!(!hide.shown(false, Some((60.0, 10.0)), 0, false));
        assert!(hide.shown(false, Some((61.0, 10.0)), 0, false));
    }

    #[test]
    fn a_hide_stays_while_the_pointer_cannot_be_read() {
        let mut hide = HiddenUntilMoved::default();
        assert!(!hide.shown(false, None, 0, false));
        assert!(!hide.shown(false, Some((5.0, 5.0)), 0, false));
        assert!(!hide.shown(false, None, 0, false));
        assert!(!hide.shown(false, Some((5.0, 5.0)), 0, false));
    }
}
