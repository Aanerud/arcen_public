//! What this host can and cannot say about the desktop's pointer.
//!
//! The Deck can draw the pointer itself, which is instant because nothing has
//! to cross the link for it — but then it is always an arrow, because it has
//! no way to know the shape. Or the compositor can draw the pointer into the
//! captured picture, which is always the right shape and always exactly as
//! late as the picture. There is no third option that is better than both, so
//! the choice belongs to the person and arrives in the handshake as a
//! `CursorMode`.
//!
//! There *appears* to be a third option: read the shape here and send its
//! name, which is what the Linux Pier does with the X cursor name and what
//! `NSCursor` looks like it offers. It does not work. Measured on this host by
//! [`probe`]: `currentSystemCursor` reports the arrow whatever the desktop is
//! showing, under every activation policy, in a process that reads all eight
//! standard cursors correctly. It reports the calling process's idea of the
//! cursor, not the desktop's. Apple deprecated it and pointed at
//! `SCStreamConfiguration.showsCursor` — compositing — for exactly this
//! reason.
//!
//! What remains here is the probe that establishes that, so the claim can be
//! re-checked on a future macOS rather than taken on trust.

use std::time::Duration;

use arcen_protocol::messages::CursorShapeKind;
// `currentSystemCursor` and some of the resize cursors carry deprecation
// warnings. Apple's replacement advice for the first is to composite the
// cursor into the capture with `SCStreamConfiguration.showsCursor`, which is a
// different design: it ties pointer movement to the video's latency, where
// this keeps the pointer local and only the shape remote. The Deck renders
// these as native compositor cursors precisely so movement is not repaint
// gated. Deprecated here means superseded for the common case, not removed.
#[allow(deprecated)]
use objc2_app_kit::NSCursor;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

/// Connects this process to the window server, without appearing anywhere.
///
/// [`NSCursor`]'s standard-cursor accessors return NULL until an `NSApplication`
/// exists, and the bindings turn that NULL into a panic. Measured on the lab
/// Mac: a session logged `cannot read the desktop cursor shape` because every
/// accessor came back NULL, even running as a proper Aqua `LaunchAgent`.
///
/// The activation policy is what makes this safe for a background agent.
/// `Prohibited` is the programmatic equivalent of `LSUIElement`: no Dock icon,
/// no menu bar, no window. The application is never run, so this brings up
/// no event loop of its own and does not compete with the runtime that serves
/// the session.
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
        // Probed before anything else is touched. Without a connection to the
        // window server the standard-cursor accessors return NULL, and the
        // bindings turn a NULL from a non-nullable return into a panic rather
        // than a `None`. `currentSystemCursor` is the one accessor declared
        // nullable, so it is the only safe way to ask whether the rest will
        // answer at all — a process with no Aqua session, or a test binary
        // without AppKit, has no cursor to watch and should say so.
        NSCursor::currentSystemCursor()?;
        // Caught rather than trusted. The standard-cursor accessors are
        // declared non-nullable and the bindings turn a NULL from one into a
        // panic, and they do return NULL in a process that has not brought up
        // AppKit. This host must not bring up AppKit: it runs as a background
        // agent with no Dock presence, and an NSApplication would give it one.
        // So the accessors are called defensively, and a host that cannot name
        // the shapes reports none rather than taking the session down with it.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(Self::capture_standard))
            .ok()
            .flatten()
    }

    /// Reads the standard cursors. May panic if `AppKit` is unavailable; see
    /// [`ShapeCatalogue::capture`], which is the only caller and guards it.
    #[allow(deprecated)]
    fn capture_standard() -> Option<Self> {
        // SAFETY: these are AppKit's standard cursor singletons. Reading them
        // allocates nothing the caller owns beyond the returned retained
        // reference, which `Retained` releases.
        use objc2_app_kit::{NSCursorFrameResizeDirections, NSCursorFrameResizePosition};

        // Window frames resize from any edge on modern macOS, and the cursors
        // for that come from `frameResizeCursorFromPosition:inDirections:`
        // rather than from the old resize singletons. Leaving them out is what
        // made a corner drag show a plain arrow while the side edges worked:
        // measured as 86 distinct cursor images against 59 that could be named.
        let frame = |position: NSCursorFrameResizePosition, kind: CursorShapeKind| {
            (
                NSCursor::frameResizeCursorFromPosition_inDirections(
                    position,
                    NSCursorFrameResizeDirections::All,
                ),
                kind,
            )
        };
        let standard: Vec<(objc2::rc::Retained<NSCursor>, CursorShapeKind)> = vec![
            (NSCursor::arrowCursor(), CursorShapeKind::Default),
            (NSCursor::IBeamCursor(), CursorShapeKind::Text),
            (NSCursor::pointingHandCursor(), CursorShapeKind::Pointer),
            (NSCursor::crosshairCursor(), CursorShapeKind::Crosshair),
            (NSCursor::openHandCursor(), CursorShapeKind::Grab),
            (NSCursor::closedHandCursor(), CursorShapeKind::Grabbing),
            (
                NSCursor::operationNotAllowedCursor(),
                CursorShapeKind::NotAllowed,
            ),
            (NSCursor::zoomInCursor(), CursorShapeKind::ZoomIn),
            (NSCursor::zoomOutCursor(), CursorShapeKind::ZoomOut),
            // Edges in both spellings. Both are in circulation: the singletons
            // still appear inside views, the frame cursors on window chrome.
            (NSCursor::resizeUpDownCursor(), CursorShapeKind::ResizeNs),
            (NSCursor::resizeLeftRightCursor(), CursorShapeKind::ResizeEw),
            (NSCursor::rowResizeCursor(), CursorShapeKind::ResizeNs),
            (NSCursor::columnResizeCursor(), CursorShapeKind::ResizeEw),
            frame(NSCursorFrameResizePosition::Top, CursorShapeKind::ResizeNs),
            frame(
                NSCursorFrameResizePosition::Bottom,
                CursorShapeKind::ResizeNs,
            ),
            frame(NSCursorFrameResizePosition::Left, CursorShapeKind::ResizeEw),
            frame(
                NSCursorFrameResizePosition::Right,
                CursorShapeKind::ResizeEw,
            ),
            // The corners, which is what "I did not get the size" was about.
            frame(
                NSCursorFrameResizePosition::TopLeft,
                CursorShapeKind::ResizeNwse,
            ),
            frame(
                NSCursorFrameResizePosition::BottomRight,
                CursorShapeKind::ResizeNwse,
            ),
            frame(
                NSCursorFrameResizePosition::TopRight,
                CursorShapeKind::ResizeNesw,
            ),
            frame(
                NSCursorFrameResizePosition::BottomLeft,
                CursorShapeKind::ResizeNesw,
            ),
        ];
        let mut known = Vec::with_capacity(standard.len());
        for (cursor, kind) in standard {
            if let Some(bytes) = image_bytes(&cursor) {
                // First spelling wins: several cursors share an image, and the
                // earlier entry is the one whose name reads better in a log.
                if !known.iter().any(|(seen, _)| seen == &bytes) {
                    known.push((bytes, kind));
                }
            }
        }
        // A catalogue that recognises only the arrow would report every cursor
        // as an arrow, which is exactly the behaviour this replaces.
        if known.len() < 2 {
            return None;
        }
        Some(Self { known })
    }

    /// Names the cursor the desktop is showing now.
    #[allow(deprecated)]
    fn current(&self) -> CursorShapeKind {
        // SAFETY: `currentSystemCursor` reads AppKit's current cursor and
        // returns a retained reference or nothing.
        let Some(cursor) = NSCursor::currentSystemCursor() else {
            return CursorShapeKind::Default;
        };
        let Some(bytes) = image_bytes(&cursor) else {
            return CursorShapeKind::Default;
        };
        self.known
            .iter()
            .find(|(known, _)| known == &bytes)
            .map_or(CursorShapeKind::Default, |(_, kind)| *kind)
    }
}

/// Returns the bytes of a cursor's image, for comparison only.
fn image_bytes(cursor: &NSCursor) -> Option<Vec<u8>> {
    // SAFETY: every NSCursor has an image; TIFF representation is a plain
    // serialisation of it and borrows nothing from the caller.
    let data = cursor.image().TIFFRepresentation()?;
    Some(data.to_vec())
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

/// What this machine can tell a Deck about its cursor.
#[derive(Debug, serde::Serialize)]
pub struct CursorProbe {
    /// Whether a cursor could be read after connecting to the window server.
    pub window_server: bool,
    /// Whether the desktop answered with a cursor at all.
    pub current_cursor_readable: bool,
    /// How many standard shapes were identified by image.
    pub shapes_known: usize,
    /// Distinct cursor *images* seen, by digest.
    ///
    /// Reported separately from the named shapes because the two answer
    /// different questions, and conflating them produced a wrong conclusion
    /// once already: an image that matches no known cursor is reported as the
    /// default arrow, which is indistinguishable from the cursor genuinely
    /// being an arrow. If this count is greater than one while `shapes_seen`
    /// holds only "Default", the cursor is changing and the matching is what
    /// is broken.
    pub distinct_images: usize,
    /// Every distinct shape seen while sweeping the pointer.
    ///
    /// This is the field that matters. More than one entry would mean the
    /// shape can be tracked and reported to a Deck for it to draw locally; a
    /// single entry across a sweep of the whole desktop means it cannot, which
    /// is what this host measures.
    pub shapes_seen: Vec<String>,
}

/// Reports whether this host could name the shape of the desktop's pointer.
///
/// Sweeps the pointer while sampling, so the answer covers the question that
/// matters — whether the shape is seen to *change* — rather than only whether
/// one can be read at all. A reader that names every cursor "default" passes
/// the simpler check and is useless.
///
/// Must run on the main thread, because connecting to the window server does.
#[must_use]
#[allow(deprecated)]
pub fn probe(sample_for: Duration) -> CursorProbe {
    let window_server = connect_to_window_server();
    let current_cursor_readable = NSCursor::currentSystemCursor().is_some();
    let catalogue = ShapeCatalogue::capture();
    let mut seen: Vec<String> = Vec::new();
    let mut images: Vec<(usize, u64)> = Vec::new();
    if let Some(catalogue) = catalogue.as_ref() {
        let deadline = std::time::Instant::now() + sample_for;
        let mut step = 0_u32;
        while std::time::Instant::now() < deadline {
            crate::input::move_pointer_for_probe(sweep_point(step));
            step = step.wrapping_add(1);
            std::thread::sleep(POLL_INTERVAL);
            let shape = format!("{:?}", catalogue.current());
            if !seen.contains(&shape) {
                seen.push(shape);
            }
            if let Some(digest) = current_image_digest() {
                if !images.contains(&digest) {
                    images.push(digest);
                }
            }
        }
    }
    CursorProbe {
        window_server,
        current_cursor_readable,
        shapes_known: catalogue.map_or(0, |c| c.known.len()),
        distinct_images: images.len(),
        shapes_seen: seen,
    }
}

/// A cheap digest of the current cursor's image, for telling images apart.
#[allow(deprecated)]
fn current_image_digest() -> Option<(usize, u64)> {
    let cursor = NSCursor::currentSystemCursor()?;
    let bytes = image_bytes(&cursor)?;
    // Length plus a rolling sum is enough to distinguish cursors; this exists
    // to count distinct images, not to identify them.
    let sum = bytes.iter().fold(0_u64, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(u64::from(*byte))
    });
    Some((bytes.len(), sum))
}

/// Watches the desktop cursor and reports shape changes.
///
/// Polls, because nothing notifies. The reference implementation this was
/// checked against does the same — its own log vocabulary has "cursor polling
/// resumed" and "cursor polling suspended" — and it reads exactly these
/// properties: `currentSystemCursor`, its `image`, and its `hotSpot`.
///
/// Shapes are named by comparing the image against the standard cursors. A
/// cursor that matches none of them is reported as the default arrow, which is
/// the same answer the Deck would have drawn anyway.
#[derive(Debug)]
pub struct CursorWatcher {
    // Behind a mutex so a session future holding a reference stays `Send`;
    // only the session task ever takes from it, so the lock is uncontended.
    shapes: std::sync::Mutex<std::sync::mpsc::Receiver<CursorShapeKind>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Distinct cursor images seen, whether or not they were named.
    ///
    /// Separated from the named shapes because conflating them produced a
    /// wrong conclusion once: an unmatched image reads as the default arrow,
    /// which is indistinguishable from the cursor really being an arrow. If
    /// this climbs while the Deck never sees a shape change, the polling works
    /// and the matching is what is broken.
    images_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    sequence: std::sync::atomic::AtomicU64,
}

impl CursorWatcher {
    /// Starts watching, or returns `None` when the shapes cannot be read.
    #[must_use]
    pub fn start() -> Option<Self> {
        let catalogue = ShapeCatalogue::capture()?;
        let (sender, shapes) = std::sync::mpsc::sync_channel(4);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let images_seen = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let thread_cancelled = std::sync::Arc::clone(&cancelled);
        let thread_images = std::sync::Arc::clone(&images_seen);
        std::thread::Builder::new()
            .name("arcen-cursor".to_owned())
            .spawn(move || {
                let mut last_shape: Option<CursorShapeKind> = None;
                let mut last_image: Option<(usize, u64)> = None;
                while !thread_cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    // Each poll's AppKit objects are autoreleased. A plain
                    // thread has no pool that drains until it exits, so without
                    // one per iteration every cursor image read for the whole
                    // session stays allocated.
                    objc2::rc::autoreleasepool(|_| {
                        let digest = current_image_digest();
                        if digest.is_some() && digest != last_image {
                            last_image = digest;
                            thread_images.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        let shape = catalogue.current();
                        if last_shape != Some(shape) && sender.try_send(shape).is_ok() {
                            last_shape = Some(shape);
                        }
                    });
                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .ok()?;
        Some(Self {
            shapes: std::sync::Mutex::new(shapes),
            cancelled,
            images_seen,
            sequence: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Returns the newest shape change, if there has been one.
    ///
    /// Never blocks, and skips superseded shapes: a pointer's shape is state,
    /// and sending the ones it has already left costs the writer for nothing.
    pub fn take_changed(&self) -> Option<arcen_protocol::messages::CursorShapeMsg> {
        let mut newest = None;
        if let Ok(shapes) = self.shapes.lock() {
            while let Ok(shape) = shapes.try_recv() {
                newest = Some(shape);
            }
        }
        Some(arcen_protocol::messages::CursorShapeMsg {
            msg_type: arcen_protocol::messages::CURSOR_SHAPE.to_owned(),
            shape: newest?,
            // Starts at one: the Deck ignores any sequence not greater than the
            // last applied, and zero is what an absent field deserialises to.
            sequence: self
                .sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1,
        })
    }

    /// How many distinct cursor images this session has seen.
    #[must_use]
    pub fn images_seen(&self) -> u64 {
        self.images_seen.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for CursorWatcher {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::{ShapeCatalogue, sweep_point};
    use arcen_protocol::messages::CursorShapeKind;

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
        let catalogue = ShapeCatalogue { known: Vec::new() };
        assert_eq!(catalogue.current(), CursorShapeKind::Default);
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
}
