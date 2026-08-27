//! Host cursor-shape watcher for the Windows session.
//!
//! Polls `GetCursorInfo` at ~20 Hz, detects when the active `HCURSOR` changes,
//! maps it against the standard system cursor handles loaded with
//! `LoadCursorW(None, IDC_*)`, and sends a [`CursorShapeMsg`] to the client
//! whenever the shape changes. Only active while [`CursorMode::Local`] is
//! negotiated.
//!
//! Runs in a dedicated OS thread (not a Tokio task) to avoid blocking Tokio
//! workers. The thread exits when the `tokio::sync::mpsc::Sender` is closed
//! (session ends).

use tokio::sync::mpsc;

#[cfg(windows)]
use arcen_protocol::messages::{CursorShapeKind, CursorShapeMsg, CURSOR_SHAPE};

/// How often to poll `GetCursorInfo`.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25); // ~40 Hz

/// Start the cursor watcher thread. Returns a receiver from which the caller
/// drains [`CursorShapeMsg`]s serialised to JSON strings whenever the cursor
/// shape changes. The thread exits when the receiver is dropped.
pub fn spawn() -> Option<mpsc::Receiver<String>> {
    let (tx, rx) = mpsc::channel::<String>(32);
    std::thread::Builder::new()
        .name("arcen-cursor-watcher".to_string())
        .spawn(move || run_watcher(tx))
        .ok()?;
    Some(rx)
}

#[cfg(windows)]
fn run_watcher(tx: mpsc::Sender<String>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorInfo, LoadCursorW, CURSORINFO, HCURSOR, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS,
        IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE,
        IDC_SIZEWE, IDC_UPARROW, IDC_WAIT,
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    // Load known system cursor handles once. A failed load yields a null
    // handle that never matches any real cursor — safe fallback.
    macro_rules! load {
        ($idc:expr) => {
            // SAFETY: LoadCursorW with None module handle and a standard IDC_*
            // resource identifier is always safe.
            unsafe { LoadCursorW(None, $idc) }.unwrap_or_default()
        };
    }

    let map: [(HCURSOR, CursorShapeKind); 14] = [
        (load!(IDC_ARROW), CursorShapeKind::Default),
        (load!(IDC_IBEAM), CursorShapeKind::Text),
        (load!(IDC_WAIT), CursorShapeKind::Wait),
        (load!(IDC_CROSS), CursorShapeKind::Crosshair),
        (load!(IDC_UPARROW), CursorShapeKind::Default),
        (load!(IDC_SIZEALL), CursorShapeKind::ResizeAll),
        (load!(IDC_SIZENESW), CursorShapeKind::ResizeNesw),
        (load!(IDC_SIZENWSE), CursorShapeKind::ResizeNwse),
        (load!(IDC_SIZEWE), CursorShapeKind::ResizeEw),
        (load!(IDC_SIZENS), CursorShapeKind::ResizeNs),
        (load!(IDC_HAND), CursorShapeKind::Pointer),
        (load!(IDC_APPSTARTING), CursorShapeKind::Progress),
        (load!(IDC_NO), CursorShapeKind::NotAllowed),
        (load!(IDC_HELP), CursorShapeKind::Help),
    ];

    let hcursor_now = || -> HCURSOR {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: info is initialised with the correct cbSize field.
        if unsafe { GetCursorInfo(&mut info) }.is_ok() {
            info.hCursor
        } else {
            HCURSOR::default()
        }
    };

    let cursor_to_shape = |h: HCURSOR| -> CursorShapeKind {
        map.iter()
            .find(|(handle, _)| *handle == h)
            .map_or(CursorShapeKind::Default, |(_, shape)| *shape)
    };

    let send = |shape: CursorShapeKind| -> bool {
        let msg = CursorShapeMsg {
            msg_type: CURSOR_SHAPE.to_owned(),
            shape,
            sequence: SEQUENCE.fetch_add(1, Ordering::Relaxed),
        };
        match serde_json::to_string(&msg) {
            Ok(json) => tx.blocking_send(json).is_ok(),
            Err(_) => true, // serialisation failure is not a session error
        }
    };

    // Seed last_cursor and send the initial shape before polling starts.
    let mut last_cursor = hcursor_now();
    if !send(cursor_to_shape(last_cursor)) {
        return;
    }

    loop {
        std::thread::sleep(POLL_INTERVAL);

        let current = hcursor_now();
        if current == last_cursor {
            continue;
        }
        last_cursor = current;

        if !send(cursor_to_shape(current)) {
            break; // receiver closed — session ended
        }
    }
}

// On non-Windows (macOS/Linux dev-machine checks): no-op so the module
// compiles everywhere without extra cfg gates at the call site.
#[cfg(not(windows))]
fn run_watcher(_tx: mpsc::Sender<String>) {}
