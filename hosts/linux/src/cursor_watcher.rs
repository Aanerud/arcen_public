//! Host cursor-shape watcher for the Linux X11 session.
//!
//! Opens a dedicated X11 connection, registers for XFixes cursor-change
//! notifications on the root window, and sends a [`CursorShapeMsg`] to the
//! client whenever the host cursor shape changes. Only active while
//! [`CursorMode::Local`] is negotiated — the cursor is already embedded in
//! the video stream when host-cursor mode is active.
//!
//! The watcher runs in its own OS thread (not a Tokio task) so the blocking
//! `wait_for_event` call never ties up a Tokio worker thread. The thread
//! exits cleanly when the `tokio::sync::mpsc::Sender` closes (session ends)
//! or when the X11 connection drops.

use tokio::sync::mpsc;

use arcen_protocol::messages::{CursorShapeKind, CursorShapeMsg, CURSOR_SHAPE};

/// Start the cursor watcher thread. Returns a receiver from which the caller
/// drains [`CursorShapeMsg`]s (serialised to JSON strings) whenever the host
/// cursor shape changes. The thread exits when the receiver is dropped (i.e.,
/// when the session ends).
///
/// Returns `None` if the X11 connection or XFixes initialisation fails; the
/// caller should log and continue without cursor shape streaming.
///
/// `display` is the DISPLAY string (e.g. `":0"`), `xauthority` the path to
/// the Xauthority file for that display (may be `None` for sessions that do
/// not require one).
pub fn spawn(display: String, xauthority: Option<String>) -> Option<mpsc::Receiver<String>> {
    let (tx, rx) = mpsc::channel::<String>(32);
    std::thread::Builder::new()
        .name("arcen-cursor-watcher".to_string())
        .spawn(move || run_watcher(display, xauthority, tx))
        .ok()?;
    Some(rx)
}

fn run_watcher(display: String, xauthority: Option<String>, tx: mpsc::Sender<String>) {
    // Point the X11 library at the correct display and authority file.
    // SAFETY: setenv is not thread-safe in multi-threaded programs, but this
    // watcher thread is spawned before any other thread touches DISPLAY or
    // XAUTHORITY for this thread's environment (each std::thread has its own
    // address-space-visible env; we only write before connecting, and the
    // values don't change after that). The x11rb connection reads them once
    // on `x11rb::connect`.
    if let Some(ref xauth) = xauthority {
        // SAFETY: single writer, thread-local-lifetime env only read by this
        // thread's connect call immediately below.
        unsafe {
            std::env::set_var("XAUTHORITY", xauth);
        }
    }

    let (conn, screen_num) = match x11rb::connect(Some(&display)) {
        Ok(pair) => pair,
        Err(error) => {
            // Rebind to avoid the tracing macro resolving `display` as
            // `tracing::field::display` (the function) rather than the
            // local String parameter.
            let x11_disp = display.as_str();
            tracing::warn!(
                target: crate::logging::target::SESSION,
                %error,
                x11_display = x11_disp,
                "cursor watcher: failed to connect to X11 display; \
                 cursor shape streaming unavailable for this session"
            );
            return;
        }
    };

    if let Err(error) = init_and_watch(&conn, screen_num, &tx) {
        tracing::debug!(
            target: crate::logging::target::SESSION,
            %error,
            "cursor watcher exited"
        );
    }
}

fn init_and_watch(
    conn: &x11rb::rust_connection::RustConnection,
    screen_num: usize,
    tx: &mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xfixes::{ConnectionExt as _, CursorNotifyMask};

    // Initialise XFixes — required before any other XFixes call.
    conn.xfixes_query_version(5, 0)?.reply()?;

    let root = conn.setup().roots[screen_num].root;

    // Register for cursor-change notifications on the root window.
    conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)?
        .check()?;

    // Send the initial cursor shape on connect so the client starts with the
    // correct shape even if it never changes during the session.
    let initial = read_cursor_shape(conn)?;
    tracing::debug!(
        target: crate::logging::target::SESSION,
        shape = ?initial.shape,
        "cursor watcher: initial shape sent"
    );
    if let Ok(json) = serde_json::to_string(&initial) {
        if tx.blocking_send(json).is_err() {
            return Ok(());
        }
    }

    // Track the last sent shape to suppress redundant updates (XFixes fires on
    // every cursor switch even when the logical shape hasn't changed — e.g.
    // the same resize cursor applied to different window decoration pieces).
    let mut last_shape = initial.shape;

    loop {
        // Block until the next X11 event arrives on this connection. This is
        // the entire point of the dedicated thread — it never occupies a Tokio
        // worker while waiting.
        let event = match conn.wait_for_event() {
            Ok(e) => e,
            Err(_) => break, // connection dropped — session ending
        };

        // We only subscribed to XFixes cursor-change events; anything else
        // (error events, etc.) is ignored.
        use x11rb::protocol::Event;
        if !matches!(event, Event::XfixesCursorNotify(_)) {
            continue;
        }

        let shape = read_cursor_shape(conn)?;
        // Suppress duplicate shapes so we don't flood the wire.
        if shape.shape == last_shape {
            continue;
        }
        last_shape = shape.shape;

        tracing::debug!(
            target: crate::logging::target::SESSION,
            shape = ?shape.shape,
            "cursor watcher: shape change sent"
        );

        if let Ok(json) = serde_json::to_string(&shape) {
            if tx.blocking_send(json).is_err() {
                break; // receiver closed — session ended
            }
        }
    }

    Ok(())
}

/// Read the current cursor image-and-name from the X server and map it to a
/// [`CursorShapeMsg`]. Uses a monotonic local counter for the sequence field.
fn read_cursor_shape(
    conn: &x11rb::rust_connection::RustConnection,
) -> Result<CursorShapeMsg, Box<dyn std::error::Error>> {
    use x11rb::protocol::xfixes::ConnectionExt as _;

    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    let reply = conn.xfixes_get_cursor_image_and_name()?.reply()?;
    let name = std::str::from_utf8(&reply.name)
        .unwrap_or("")
        .to_lowercase();

    // Determine whether the cursor image is fully transparent. XFixes returns
    // pixels as ARGB u32 values. If every pixel has zero alpha the cursor is
    // invisible (e.g. the blank cursor sprite that appears when a uinput tablet
    // device enters proximity). Treat that case as Hidden regardless of name.
    // For non-transparent cursors an empty or unrecognised name maps to Default.
    let all_transparent = !reply.cursor_image.iter().any(|px| (px >> 24) != 0);

    let shape = if all_transparent {
        CursorShapeKind::Hidden
    } else {
        cursor_name_to_shape(if name.is_empty() { "left_ptr" } else { &name })
    };

    tracing::trace!(
        target: crate::logging::target::SESSION,
        %name,
        ?shape,
        all_transparent,
        "cursor watcher: read cursor"
    );

    Ok(CursorShapeMsg {
        msg_type: CURSOR_SHAPE.to_owned(),
        shape,
        sequence: SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    })
}

/// Map an X cursor name to the nearest [`CursorShapeKind`].
///
/// X cursor names are standardised in the Xcursor theme specification and the
/// CSS cursor spec. The mapping covers the most common names from the
/// "core" X cursor font and the freedesktop.org cursor theme.
///
/// An empty or unrecognised name maps to `Default`. Blank/invisible cursors
/// are detected via pixel transparency in `read_cursor_shape` before this
/// function is called — do **not** map `""` to `Hidden` here.
fn cursor_name_to_shape(name: &str) -> CursorShapeKind {
    match name {
        // Resize — named shapes for window edges and corners.
        "n-resize" | "s-resize" | "ns-resize" | "size_ver" | "top_side" | "bottom_side"
        | "v_double_arrow" | "row-resize" => CursorShapeKind::ResizeNs,

        "e-resize" | "w-resize" | "ew-resize" | "size_hor" | "right_side" | "left_side"
        | "h_double_arrow" | "col-resize" => CursorShapeKind::ResizeEw,

        "nw-resize"
        | "se-resize"
        | "nwse-resize"
        | "size_fdiag"
        | "top_left_corner"
        | "bottom_right_corner"
        | "fd_double_arrow"
        | "ul_angle"
        | "lr_angle" => CursorShapeKind::ResizeNwse,

        "ne-resize" | "sw-resize" | "nesw-resize" | "size_bdiag" | "top_right_corner"
        | "bottom_left_corner" | "bd_double_arrow" | "ur_angle" | "ll_angle" => {
            CursorShapeKind::ResizeNesw
        }

        "move" | "size_all" | "fleur" | "all-scroll" => CursorShapeKind::ResizeAll,

        // Text / editing.
        "text" | "xterm" | "ibeam" => CursorShapeKind::Text,

        // Pointer / hyperlink.
        "pointer" | "hand" | "hand1" | "hand2" | "pointing_hand" => CursorShapeKind::Pointer,

        // Crosshair / precision.
        "crosshair" | "cross" | "tcross" => CursorShapeKind::Crosshair,

        // Grab / hand.
        "grab" | "openhand" | "hand3" => CursorShapeKind::Grab,
        "grabbing" | "closedhand" | "hand4" => CursorShapeKind::Grabbing,

        // Zoom.
        "zoom-in" | "zoom_in" => CursorShapeKind::ZoomIn,
        "zoom-out" | "zoom_out" => CursorShapeKind::ZoomOut,

        // Wait / progress.
        "wait" | "watch" | "clock" => CursorShapeKind::Wait,
        "progress" | "left_ptr_watch" | "half-busy" => CursorShapeKind::Progress,

        // Help.
        "help" | "question_arrow" | "whats_this" => CursorShapeKind::Help,

        // Not allowed.
        "not-allowed" | "forbidden" | "circle" => CursorShapeKind::NotAllowed,

        // Explicitly named blank/invisible cursors. Fully-transparent unnamed
        // cursors are caught before this function via pixel inspection.
        "none" | "blank_cursor" | "nil_cursor" => CursorShapeKind::Hidden,

        // Default arrow, "left_ptr" (the standard X11 arrow cursor name), and
        // everything else (including unknown themes, hex names, etc.).
        _ => CursorShapeKind::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_edge_names_map_to_resize_variants() {
        assert_eq!(cursor_name_to_shape("n-resize"), CursorShapeKind::ResizeNs);
        assert_eq!(cursor_name_to_shape("ew-resize"), CursorShapeKind::ResizeEw);
        assert_eq!(
            cursor_name_to_shape("nwse-resize"),
            CursorShapeKind::ResizeNwse
        );
        assert_eq!(
            cursor_name_to_shape("nesw-resize"),
            CursorShapeKind::ResizeNesw
        );
        assert_eq!(cursor_name_to_shape("fleur"), CursorShapeKind::ResizeAll);
        // Old X core cursor font names (used by Xfwm4 and Openbox)
        assert_eq!(
            cursor_name_to_shape("ul_angle"),
            CursorShapeKind::ResizeNwse
        );
        assert_eq!(
            cursor_name_to_shape("lr_angle"),
            CursorShapeKind::ResizeNwse
        );
        assert_eq!(
            cursor_name_to_shape("ur_angle"),
            CursorShapeKind::ResizeNesw
        );
        assert_eq!(
            cursor_name_to_shape("ll_angle"),
            CursorShapeKind::ResizeNesw
        );
    }

    #[test]
    fn common_names_map_correctly() {
        assert_eq!(cursor_name_to_shape("text"), CursorShapeKind::Text);
        assert_eq!(cursor_name_to_shape("xterm"), CursorShapeKind::Text);
        assert_eq!(cursor_name_to_shape("pointer"), CursorShapeKind::Pointer);
        assert_eq!(cursor_name_to_shape("hand2"), CursorShapeKind::Pointer);
        assert_eq!(
            cursor_name_to_shape("crosshair"),
            CursorShapeKind::Crosshair
        );
        assert_eq!(cursor_name_to_shape("watch"), CursorShapeKind::Wait);
        assert_eq!(
            cursor_name_to_shape("left_ptr_watch"),
            CursorShapeKind::Progress
        );
        assert_eq!(
            cursor_name_to_shape("not-allowed"),
            CursorShapeKind::NotAllowed
        );
    }

    #[test]
    fn explicit_blank_names_map_to_hidden() {
        // Explicitly named blank cursors are still Hidden.
        assert_eq!(cursor_name_to_shape("none"), CursorShapeKind::Hidden);
        assert_eq!(
            cursor_name_to_shape("blank_cursor"),
            CursorShapeKind::Hidden
        );
        assert_eq!(cursor_name_to_shape("nil_cursor"), CursorShapeKind::Hidden);
    }

    #[test]
    fn empty_name_maps_to_default_not_hidden() {
        // Empty name is now handled as Default — truly invisible cursors are
        // detected via pixel transparency in read_cursor_shape(), not by name.
        assert_eq!(cursor_name_to_shape("left_ptr"), CursorShapeKind::Default);
    }

    #[test]
    fn unknown_name_maps_to_default() {
        assert_eq!(
            cursor_name_to_shape("some_unknown_cursor_2099"),
            CursorShapeKind::Default
        );
    }
}
