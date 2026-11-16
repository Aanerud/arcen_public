// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Clipboard (owner: `platform` stream).
//!
//! [`WindowsClipboard`] is `arcen_deck_core::clipboard::PlatformClipboard`
//! on a message-only window of the UI thread: `CF_UNICODETEXT` for text, a
//! registered `PNG` format for images, and `CF_DIBV5`/`CF_DIB` converted to
//! PNG by the shared `arcen_media::clipboard::dib_to_png` when an
//! application offers no PNG (the Windows twin of the macOS Deck's
//! TIFF → PNG). A write also offers `CF_DIBV5`, so applications that only
//! paste bitmaps (Paint) take a host's image.
//!
//! Echoes: every write carries Arcen's origin marker in its own registered
//! format, and the write itself does not move [`PlatformClipboard::change_token`]
//! (the macOS Deck re-reads `changeCount` after its own write for the same
//! reason), so the controller never reads back what it just wrote.
//!
//! `AddClipboardFormatListener` raises a flag (and an optional waker) on
//! `WM_CLIPBOARDUPDATE`, so a shell can poll only when something changed.
//! The window's messages arrive through the UI thread's message loop.

#![allow(
    unsafe_code,
    reason = "Win32 is a C API; every unsafe block states its contract"
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arcen_deck_core::clipboard::PlatformClipboard;
use arcen_media::clipboard::{
    ECHO_MARKER_BYTES, HARD_MAX_CLIPBOARD_BYTES, ImageLimits, MAX_DECODED_IMAGE_BYTES, dib_to_png,
    png_to_dibv5,
};
use arcen_protocol::messages::ClipboardContentKind;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HWND_MESSAGE,
    RegisterClassExW, SetWindowLongPtrW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE,
    WNDCLASSEXW,
};
use windows::core::{PCWSTR, w};

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
/// Retry delays for `OpenClipboard`, which fails while another process
/// holds the clipboard; short because this runs on the UI thread.
const OPEN_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(5),
    Duration::from_millis(10),
    Duration::from_millis(20),
];
/// The registered name of Arcen's origin-marker format on a Deck.
const ORIGIN_FORMAT_NAME: PCWSTR = w!("ArcenDeckClipboardOrigin");

/// Something to call when the clipboard changes (for example a repaint
/// request). Called on the UI thread from the window procedure.
pub type ClipboardWaker = Box<dyn Fn() + Send + Sync>;

struct ListenerState {
    changed: AtomicBool,
    waker: OnceLock<ClipboardWaker>,
}

/// The system clipboard, owned by the UI thread.
pub struct WindowsClipboard {
    window: HWND,
    state: Arc<ListenerState>,
    origin_format: u32,
    png_format: u32,
    /// The sequence number our own last write left behind.
    own_write_sequence: Option<u32>,
    /// The sequence number last observed and the token reported for it.
    observed_sequence: u32,
    token: u64,
    /// A read found the clipboard held open by another process.
    read_blocked: std::cell::Cell<bool>,
}

impl std::fmt::Debug for WindowsClipboard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsClipboard")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl WindowsClipboard {
    /// **UI thread.** Creates the listener window; `None` when the window
    /// station has no clipboard or the window cannot be created.
    #[must_use]
    pub fn open() -> Option<Self> {
        // SAFETY: registering a static, NUL-terminated format name.
        let origin_format = unsafe { RegisterClipboardFormatW(ORIGIN_FORMAT_NAME) };
        // SAFETY: as above; "PNG" is the name Windows applications use.
        let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        if origin_format == 0 || png_format == 0 {
            return None;
        }
        let window = create_listener_window()?;
        let state = Arc::new(ListenerState {
            changed: AtomicBool::new(false),
            waker: OnceLock::new(),
        });
        let raw = Arc::into_raw(Arc::clone(&state));
        // SAFETY: `window` is ours; the pointer is reclaimed in Drop after
        // the window is destroyed.
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, raw as isize) };
        // SAFETY: `window` is a live window of this thread.
        let _ = unsafe { AddClipboardFormatListener(window) };
        // SAFETY: no arguments.
        let sequence = unsafe { GetClipboardSequenceNumber() };
        Some(Self {
            window,
            state,
            origin_format,
            png_format,
            own_write_sequence: None,
            observed_sequence: sequence,
            token: u64::from(sequence),
            read_blocked: std::cell::Cell::new(false),
        })
    }

    /// Calls `waker` whenever another application changes the clipboard.
    /// Set once; later calls are ignored.
    pub fn set_change_waker(&self, waker: ClipboardWaker) {
        let _ = self.state.waker.set(waker);
    }

    /// Whether `WM_CLIPBOARDUPDATE` arrived since the last call.
    #[must_use]
    pub fn take_changed(&self) -> bool {
        self.state.changed.swap(false, Ordering::AcqRel)
    }

    fn open_clipboard(&self) -> Option<ClipboardGuard> {
        let attempt = || {
            // SAFETY: `self.window` is a live window of this thread.
            unsafe { OpenClipboard(self.window) }.is_ok()
        };
        if attempt() {
            return Some(ClipboardGuard);
        }
        for delay in OPEN_RETRY_DELAYS {
            std::thread::sleep(delay);
            if attempt() {
                return Some(ClipboardGuard);
            }
        }
        None
    }

    /// [`Self::open_clipboard`] for a read: a failure is remembered for
    /// [`PlatformClipboard::take_read_blocked`].
    fn open_for_read(&self) -> Option<ClipboardGuard> {
        let guard = self.open_clipboard();
        if guard.is_none() {
            self.read_blocked.set(true);
        }
        guard
    }

    fn read_png_inner(&self, max_encoded_bytes: usize) -> Option<Vec<u8>> {
        let limits = ImageLimits {
            max_encoded_bytes: max_encoded_bytes.min(HARD_MAX_CLIPBOARD_BYTES),
            ..ImageLimits::default()
        };
        let _open = self.open_for_read()?;
        if format_available(self.png_format) {
            // Refuse rather than truncate anything larger than allowed.
            if let Some(Ok(png)) = read_global(self.png_format, limits.max_encoded_bytes) {
                return Some(png);
            }
        }
        // The largest packed DIB a legal image can be: header, masks, a
        // full colour table and 32-bit rows (rows of 24-bit images are
        // smaller), plus slack for trailing profile data.
        let max_dib = MAX_DECODED_IMAGE_BYTES.saturating_add(64 * 1024);
        for format in [CF_DIBV5, CF_DIB] {
            if !format_available(format) {
                continue;
            }
            if let Some(Ok(mut dib)) = read_global(format, max_dib) {
                let png = dib_to_png(&dib, limits);
                zeroize::Zeroize::zeroize(&mut dib);
                if let Ok(png) = png {
                    return Some(png);
                }
            }
        }
        None
    }
}

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        // SAFETY: the window is ours and still alive; the user-data pointer
        // is detached before it is reclaimed.
        unsafe {
            let _ = RemoveClipboardFormatListener(self.window);
            let raw = SetWindowLongPtrW(self.window, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.window);
            if raw != 0 {
                drop(Arc::from_raw(raw as *const ListenerState));
            }
        }
    }
}

impl PlatformClipboard for WindowsClipboard {
    fn change_token(&mut self) -> u64 {
        // SAFETY: no arguments.
        let sequence = unsafe { GetClipboardSequenceNumber() };
        if sequence != self.observed_sequence {
            self.observed_sequence = sequence;
            if self.own_write_sequence != Some(sequence) {
                self.own_write_sequence = None;
                self.token = self.token.wrapping_add(1);
            }
        }
        self.token
    }

    fn read_origin_marker(&mut self) -> Option<Vec<u8>> {
        let _open = self.open_for_read()?;
        if !format_available(self.origin_format) {
            return None;
        }
        read_global(self.origin_format, ECHO_MARKER_BYTES)
            .and_then(Result::ok)
            .filter(|marker| marker.len() == ECHO_MARKER_BYTES)
    }

    fn read_text(&mut self) -> Option<zeroize::Zeroizing<String>> {
        let _open = self.open_for_read()?;
        if !format_available(CF_UNICODETEXT) {
            return None;
        }
        // Enough UTF-16 for the largest text any policy sends (each unit
        // is at least one UTF-8 byte), plus the terminator; a longer text
        // is cut there and the controller marks it truncated.
        let max_bytes = HARD_MAX_CLIPBOARD_BYTES.saturating_add(1).saturating_mul(2);
        let bytes = zeroize::Zeroizing::new(match read_global(CF_UNICODETEXT, max_bytes)? {
            Ok(bytes) | Err(TooLarge(bytes)) => bytes,
        });
        decode_clipboard_utf16(&bytes).map(zeroize::Zeroizing::new)
    }

    fn read_png(&mut self, max_encoded_bytes: usize) -> Option<Vec<u8>> {
        self.read_png_inner(max_encoded_bytes)
    }

    fn take_read_blocked(&mut self) -> bool {
        self.read_blocked.replace(false)
    }

    fn write(&mut self, origin_marker: &[u8], kind: ClipboardContentKind, bytes: &[u8]) -> bool {
        if origin_marker.len() != ECHO_MARKER_BYTES {
            return false;
        }
        // Prepare everything before taking the clipboard.
        let payloads: Vec<(u32, zeroize::Zeroizing<Vec<u8>>)> = match kind {
            ClipboardContentKind::TextUtf8 => {
                let Ok(text) = std::str::from_utf8(bytes) else {
                    return false;
                };
                vec![(
                    CF_UNICODETEXT,
                    zeroize::Zeroizing::new(encode_clipboard_utf16(text)),
                )]
            }
            ClipboardContentKind::ImagePng => {
                let Ok(dib) = png_to_dibv5(bytes, ImageLimits::default()) else {
                    return false;
                };
                vec![
                    (self.png_format, zeroize::Zeroizing::new(bytes.to_vec())),
                    (CF_DIBV5, zeroize::Zeroizing::new(dib)),
                ]
            }
        };
        let written = {
            let Some(_open) = self.open_clipboard() else {
                return false;
            };
            // SAFETY: the clipboard is open on this thread; emptying makes
            // our window its owner.
            if unsafe { EmptyClipboard() }.is_err() {
                return false;
            }
            set_global(self.origin_format, origin_marker)
                && payloads
                    .iter()
                    .all(|(format, payload)| set_global(*format, payload))
        };
        // SAFETY: no arguments.
        let sequence = unsafe { GetClipboardSequenceNumber() };
        self.own_write_sequence = Some(sequence);
        self.observed_sequence = sequence;
        written
    }
}

/// Closes the clipboard when dropped; created only after `OpenClipboard`
/// succeeded.
struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: a guard exists only while this thread holds the clipboard.
        let _ = unsafe { CloseClipboard() };
    }
}

fn format_available(format: u32) -> bool {
    // SAFETY: no pointer arguments.
    unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// The clipboard held more than the caller allowed; carries the first
/// `maximum` bytes.
struct TooLarge(Vec<u8>);

/// Copies clipboard data of `format` (clipboard open). `Err` carries the
/// first `maximum` bytes when the data is larger.
fn read_global(format: u32, maximum: usize) -> Option<Result<Vec<u8>, TooLarge>> {
    // SAFETY: the clipboard is open; the handle stays owned by the system.
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let global = HGLOBAL(handle.0);
    // SAFETY: a clipboard memory handle.
    let size = unsafe { GlobalSize(global) };
    if size == 0 {
        return None;
    }
    let take = size.min(maximum);
    // SAFETY: valid while the clipboard is open.
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return None;
    }
    let mut bytes = Vec::new();
    let copied = bytes.try_reserve_exact(take).is_ok();
    if copied {
        // SAFETY: GlobalSize bounds the locked allocation and `take` ≤ it.
        bytes.extend_from_slice(unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), take) });
    }
    // SAFETY: balances the GlobalLock above.
    let _ = unsafe { GlobalUnlock(global) };
    if !copied {
        return None;
    }
    Some(if size > maximum {
        Err(TooLarge(bytes))
    } else {
        Ok(bytes)
    })
}

/// Hands `bytes` to the clipboard as `format` (clipboard open, emptied).
fn set_global(format: u32, bytes: &[u8]) -> bool {
    // SAFETY: a movable allocation of the exact length (at least one byte).
    let Ok(global) = (unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }) else {
        return false;
    };
    // SAFETY: a fresh allocation owned here.
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        // SAFETY: ownership was never transferred.
        let _ = unsafe { GlobalFree(global) };
        return false;
    }
    // SAFETY: the destination holds at least `bytes.len()` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len()) };
    // SAFETY: balances the GlobalLock above.
    let _ = unsafe { GlobalUnlock(global) };
    // SAFETY: the clipboard is open and takes ownership only on success.
    if unsafe { SetClipboardData(format, HANDLE(global.0)) }.is_err() {
        // SAFETY: still ours after a failure.
        let _ = unsafe { GlobalFree(global) };
        return false;
    }
    true
}

/// `CF_UNICODETEXT` bytes for `text`: little-endian UTF-16 with a NUL.
#[must_use]
pub fn encode_clipboard_utf16(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Text from `CF_UNICODETEXT` bytes: up to the first NUL (the allocation
/// may be larger than the string). Invalid UTF-16 is refused, except one
/// high surrogate cut off at the very end of a read that was capped.
#[must_use]
pub fn decode_clipboard_utf16(bytes: &[u8]) -> Option<String> {
    let mut units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    if units
        .last()
        .is_some_and(|unit| (0xD800..0xDC00).contains(unit))
    {
        units.pop();
    }
    let text = String::from_utf16(&units).ok();
    zeroize::Zeroize::zeroize(&mut units);
    text.filter(|text| !text.is_empty())
}

fn create_listener_window() -> Option<HWND> {
    static CLASS: OnceLock<bool> = OnceLock::new();
    // SAFETY: the module handle of this executable.
    let instance = unsafe { GetModuleHandleW(None) }.ok()?;
    let registered = *CLASS.get_or_init(|| {
        let class = WNDCLASSEXW {
            cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or(0),
            lpfnWndProc: Some(listener_window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("ArcenDeckClipboardListener"),
            ..WNDCLASSEXW::default()
        };
        // SAFETY: a fully initialised class with a static name.
        (unsafe { RegisterClassExW(&raw const class) }) != 0
    });
    if !registered {
        return None;
    }
    // SAFETY: a message-only window of the registered class.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("ArcenDeckClipboardListener"),
            w!("Arcen Deck Clipboard"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            instance,
            None,
        )
    }
    .ok()
}

unsafe extern "system" fn listener_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLIPBOARDUPDATE {
        // SAFETY: the user data is null or the Arc pointer set in `open`,
        // which outlives the window.
        let raw = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const ListenerState;
        if !raw.is_null() {
            // SAFETY: as above; borrowed only for this call.
            let state = unsafe { &*raw };
            state.changed.store(true, Ordering::Release);
            if let Some(waker) = state.waker.get() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(waker));
            }
        }
        return LRESULT(0);
    }
    // SAFETY: everything else goes to the default procedure unchanged.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn utf16_round_trips_with_emoji_and_cjk() {
        let text = "Arcen 🚀 日本語 ✓";
        let bytes = encode_clipboard_utf16(text);
        assert_eq!(bytes.len() % 2, 0);
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]);
        assert_eq!(decode_clipboard_utf16(&bytes).as_deref(), Some(text));
    }

    #[test]
    fn utf16_stops_at_the_terminator_and_tolerates_a_capped_cut() {
        let mut bytes = encode_clipboard_utf16("ab");
        bytes.extend_from_slice(&[0x41, 0x00, 0xFF, 0xFF]);
        assert_eq!(decode_clipboard_utf16(&bytes).as_deref(), Some("ab"));

        let rocket = encode_clipboard_utf16("x🚀");
        // Cut after the high surrogate, without a terminator.
        assert_eq!(decode_clipboard_utf16(&rocket[..4]).as_deref(), Some("x"));
        // A lone low surrogate in the middle is refused.
        let bad = [0x00, 0xDC, 0x41, 0x00, 0x00, 0x00];
        assert_eq!(decode_clipboard_utf16(&bad), None);
        assert_eq!(decode_clipboard_utf16(&[0, 0]), None);
        assert_eq!(decode_clipboard_utf16(&[0x41]), None);
    }
}
