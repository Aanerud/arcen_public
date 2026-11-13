// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host cursor watcher for the Windows session.
//!
//! Polls `GetCursorInfo` at ~40 Hz and, whenever the desktop's cursor
//! changes, tells the Deck: a Deck that declared `cursor_image_v1` gets the
//! cursor's actual image (`GetIconInfo` + `GetDIBits`, colour cursors with
//! alpha, old colour cursors and monochrome ones through their masks), as a
//! `CursorUpdate` record the broker's lane relay
//! ([`arcen_protocol::cursor::CursorRecordRelay`]) puts on the input/cursor
//! lane; any other Deck gets the named shape of the standard system cursor
//! (`LoadCursorW(None, IDC_*)` handles), as before. Image ids, repeat
//! suppression and the new-image rate limit are the shared
//! [`arcen_input::cursor::HostCursor`]'s. Only active while
//! `CursorMode::Local` is negotiated.
//!
//! Runs in a dedicated OS thread (not a Tokio task) to avoid blocking Tokio
//! workers. The thread exits when every receiver is gone (session ends).

use tokio::sync::watch;

/// How often to poll `GetCursorInfo`.
#[cfg(windows)]
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25); // ~40 Hz

/// What the watcher hands the session for the Deck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorMessage {
    /// A `cursor_shape` message (JSON), for a Deck without cursor images.
    Shape(String),
    /// A `CursorUpdate` record with its image inline, for the lane relay.
    Record(Vec<u8>),
}

/// Start the cursor watcher thread. The receiver holds the newest message
/// for the Deck (a newer one replaces one not yet sent). The thread exits
/// when the receiver is dropped.
pub fn spawn(deck_draws_images: bool) -> Option<watch::Receiver<Option<CursorMessage>>> {
    let (tx, rx) = watch::channel(None);
    std::thread::Builder::new()
        .name("arcen-cursor-watcher".to_string())
        .spawn(move || run_watcher(&tx, deck_draws_images))
        .ok()?;
    Some(rx)
}

#[cfg(windows)]
fn run_watcher(tx: &watch::Sender<Option<CursorMessage>>, deck_draws_images: bool) {
    use std::time::Instant;

    use arcen_input::cursor::{HostCursor, HostCursorOutcome, HostCursorSample};
    use arcen_protocol::messages::{CursorShapeKind, CursorShapeMsg, CURSOR_SHAPE};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadCursorW, HCURSOR, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND,
        IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE,
        IDC_SIZEWE, IDC_UPARROW, IDC_WAIT, SM_MOUSEPRESENT,
    };

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
    let cursor_to_shape = |h: HCURSOR| -> CursorShapeKind {
        map.iter()
            .find(|(handle, _)| *handle == h)
            .map_or(CursorShapeKind::Default, |(_, shape)| *shape)
    };

    let mut host = HostCursor::default();
    let mut record_sequence = 0u64;
    let mut shape_sequence = 0u64;
    let mut last: Option<(bool, HCURSOR)> = None;
    let mut last_shape = None;
    let mut unreadable_logged = false;
    let mut refused_logged = false;
    loop {
        if tx.is_closed() {
            return;
        }
        // A host without a mouse keeps its cursor hidden for good (Windows
        // shows the pointer only once a mouse exists); the Deck's own
        // pointer still moves the desktop, so only an application's hide is
        // a hide there.
        // SAFETY: GetSystemMetrics has no preconditions.
        let mouse_present = unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0;
        let (showing, handle) = current_cursor(mouse_present);
        let now = Instant::now();
        let mut outcome = HostCursorOutcome::Unchanged;
        if last != Some((showing, handle)) {
            last = Some((showing, handle));
            if deck_draws_images {
                let sample = if showing {
                    // SAFETY: `handle` was just returned by GetCursorInfo;
                    // reading it copies its bitmaps and releases them.
                    match unsafe { read_cursor_image(handle) } {
                        Ok(image) => Some(HostCursorSample::Image {
                            image,
                            fallback: cursor_to_shape(handle),
                        }),
                        Err(error) => {
                            if !unreadable_logged {
                                unreadable_logged = true;
                                tracing::warn!(
                                    target: crate::logging::SESSION,
                                    %error,
                                    "cursor watcher: a cursor image could not be read; the Deck keeps the previous cursor"
                                );
                            }
                            None
                        }
                    }
                } else {
                    Some(HostCursorSample::Hidden)
                };
                if let Some(sample) = sample {
                    outcome = host.offer(sample, now);
                }
            } else {
                let shape = if showing {
                    cursor_to_shape(handle)
                } else {
                    CursorShapeKind::Hidden
                };
                if last_shape != Some(shape) {
                    last_shape = Some(shape);
                    shape_sequence += 1;
                    let msg = CursorShapeMsg {
                        msg_type: CURSOR_SHAPE.to_owned(),
                        shape,
                        sequence: shape_sequence,
                    };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        tx.send_replace(Some(CursorMessage::Shape(json)));
                    }
                }
            }
        } else if host.retry_at().is_some_and(|at| now >= at) {
            outcome = host.poll(now);
        }
        match outcome {
            HostCursorOutcome::Changed => {
                if let Some(facts) = host.current() {
                    record_sequence += 1;
                    match arcen_input::cursor::agent_cursor_record(facts, record_sequence) {
                        Ok(record) => {
                            tracing::debug!(
                                target: crate::logging::SESSION,
                                cursor = %facts.summary(),
                                images = host.registered_images(),
                                "cursor watcher: cursor changed"
                            );
                            tx.send_replace(Some(CursorMessage::Record(record)));
                        }
                        Err(error) => tracing::warn!(
                            target: crate::logging::SESSION,
                            %error,
                            "cursor watcher: cursor record could not be encoded"
                        ),
                    }
                }
            }
            HostCursorOutcome::ImageRefused(error) if !refused_logged => {
                refused_logged = true;
                tracing::warn!(
                    target: crate::logging::SESSION,
                    %error,
                    "cursor watcher: a cursor image could not be fitted to the wire; the Deck keeps the previous cursor"
                );
            }
            HostCursorOutcome::Deferred
            | HostCursorOutcome::ImageRefused(_)
            | HostCursorOutcome::Unchanged => {}
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Whether the desktop shows its cursor, and which one.
#[cfg(windows)]
fn current_cursor(mouse_present: bool) -> (bool, windows::Win32::UI::WindowsAndMessaging::HCURSOR) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorInfo, CURSORINFO, CURSOR_SHOWING, CURSOR_SUPPRESSED, HCURSOR,
    };
    let mut info = CURSORINFO {
        cbSize: u32::try_from(std::mem::size_of::<CURSORINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: info is initialised with the correct cbSize field.
    if unsafe { GetCursorInfo(&mut info) }.is_err() {
        // The input desktop is not readable (a secure desktop): keep what
        // the Deck shows rather than guessing.
        return (true, HCURSOR::default());
    }
    // A pen or touch suppresses the drawn cursor; that is not a hide.
    let shown_flags = CURSOR_SHOWING.0 | CURSOR_SUPPRESSED.0;
    let hidden_by_application = mouse_present && info.flags.0 & shown_flags == 0;
    (
        !info.hCursor.is_invalid() && !hidden_by_application,
        info.hCursor,
    )
}

/// Copies a cursor's bitmaps into a [`arcen_media::cursor::CursorImage`].
///
/// # Safety
///
/// `cursor` must be a cursor handle (it may be destroyed concurrently; the
/// calls then fail and an error is returned).
#[cfg(windows)]
unsafe fn read_cursor_image(
    cursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
) -> Result<arcen_media::cursor::CursorImage, String> {
    use arcen_media::cursor::{mask_row_bytes, CursorImage, MAX_SOURCE_CURSOR_SIDE};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, RGBQUAD,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, HICON, ICONINFO};

    /// A `BITMAPINFO` with room for a monochrome colour table.
    #[repr(C)]
    struct MonoBitmapInfo {
        header: BITMAPINFOHEADER,
        colours: [RGBQUAD; 2],
    }

    struct Bitmaps(ICONINFO);
    impl Drop for Bitmaps {
        fn drop(&mut self) {
            for bitmap in [self.0.hbmMask, self.0.hbmColor] {
                if !bitmap.is_invalid() {
                    // SAFETY: GetIconInfo created these bitmaps for the
                    // caller, who must delete them.
                    let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
                }
            }
        }
    }
    struct ScreenDc(HDC);
    impl Drop for ScreenDc {
        fn drop(&mut self) {
            // SAFETY: obtained from GetDC(null) below.
            let _ = unsafe { ReleaseDC(HWND::default(), self.0) };
        }
    }

    let size = |bitmap: HBITMAP| -> Result<(u32, u32), String> {
        let mut info = BITMAP::default();
        // SAFETY: `info` is a BITMAP of the size passed.
        let written = unsafe {
            GetObjectW(
                HGDIOBJ(bitmap.0),
                i32::try_from(std::mem::size_of::<BITMAP>()).unwrap_or(0),
                Some(std::ptr::addr_of_mut!(info).cast()),
            )
        };
        if written == 0 {
            return Err("GetObjectW failed".to_owned());
        }
        let width = u32::try_from(info.bmWidth).map_err(|_| "negative bitmap width")?;
        let height = u32::try_from(info.bmHeight).map_err(|_| "negative bitmap height")?;
        // Bounded before anything is allocated: twice the largest source
        // side covers a monochrome mask's stacked planes.
        if width == 0
            || height == 0
            || width > MAX_SOURCE_CURSOR_SIDE
            || height > MAX_SOURCE_CURSOR_SIDE * 2
        {
            return Err(format!("cursor bitmap {width}x{height} is out of range"));
        }
        Ok((width, height))
    };

    let mut icon = ICONINFO::default();
    // SAFETY: `icon` receives two bitmaps owned by `Bitmaps` from here on.
    unsafe { GetIconInfo(HICON(cursor.0), &mut icon) }.map_err(|error| error.to_string())?;
    let bitmaps = Bitmaps(icon);
    let icon = &bitmaps.0;
    // SAFETY: GetDC(null) is the screen DC, released by `ScreenDc`.
    let dc = ScreenDc(unsafe { GetDC(HWND::default()) });
    if dc.0.is_invalid() {
        return Err("GetDC failed".to_owned());
    }
    let (mask_width, mask_height) = size(icon.hbmMask)?;
    let colour_size = if icon.hbmColor.is_invalid() {
        None
    } else {
        Some(size(icon.hbmColor)?)
    };
    let (width, height) = colour_size.unwrap_or((mask_width, mask_height / 2));
    let mask_rows = if colour_size.is_some() {
        height
    } else {
        mask_height
    };
    if mask_width != width
        || mask_rows > mask_height
        || height == 0
        || height > MAX_SOURCE_CURSOR_SIDE
    {
        return Err(format!(
            "cursor bitmaps disagree: mask {mask_width}x{mask_height}, colour {colour_size:?}"
        ));
    }
    let header = |bits: u16, rows: u32| BITMAPINFOHEADER {
        biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
        biWidth: i32::try_from(width).unwrap_or(0),
        // Negative: top-down rows.
        biHeight: -i32::try_from(rows).unwrap_or(0),
        biPlanes: 1,
        biBitCount: bits,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let colour = match colour_size {
        Some(_) => {
            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            let mut info = BITMAPINFO {
                bmiHeader: header(32, height),
                ..Default::default()
            };
            // SAFETY: `pixels` holds `height` top-down 32-bit rows of
            // `width` pixels, exactly what the header asks for.
            let lines = unsafe {
                GetDIBits(
                    dc.0,
                    icon.hbmColor,
                    0,
                    height,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            };
            if u32::try_from(lines).ok() != Some(height) {
                return Err("GetDIBits (colour) failed".to_owned());
            }
            Some(pixels)
        }
        None => None,
    };
    let mut mask = vec![0u8; mask_row_bytes(width) * mask_rows as usize];
    let mut info = MonoBitmapInfo {
        header: header(1, mask_rows),
        colours: [RGBQUAD::default(); 2],
    };
    // SAFETY: `mask` holds `mask_rows` 1-bit rows padded to 32 bits, and
    // `info` has room for the two-entry colour table GetDIBits writes.
    let lines = unsafe {
        GetDIBits(
            dc.0,
            icon.hbmMask,
            0,
            mask_rows,
            Some(mask.as_mut_ptr().cast()),
            std::ptr::addr_of_mut!(info).cast::<BITMAPINFO>(),
            DIB_RGB_COLORS,
        )
    };
    if u32::try_from(lines).ok() != Some(mask_rows) {
        return Err("GetDIBits (mask) failed".to_owned());
    }
    CursorImage::from_icon_planes(
        width,
        height,
        (i64::from(icon.xHotspot), i64::from(icon.yHotspot)),
        colour.as_deref(),
        &mask,
    )
    .map_err(|error| error.to_string())
}

// On non-Windows (macOS/Linux dev-machine checks): no-op so the module
// compiles everywhere without extra cfg gates at the call site.
#[cfg(not(windows))]
fn run_watcher(_tx: &watch::Sender<Option<CursorMessage>>, _deck_draws_images: bool) {}
