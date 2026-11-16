// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The host's cursor image as the window's cursor over the video.
//!
//! The shell sizes the image for the window's DPI (the shared
//! `arcen_media::cursor::CursorImage::present`, as the macOS Deck's sharp
//! cursor does) and hands it here; this makes it an `HCURSOR` and answers
//! `WM_SETCURSOR` with it while the pointer is over the image. Hidden means
//! no cursor there; none means the system arrow.

use arcen_input::cursor::DeckCursor;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyCursor, HCURSOR, HTCLIENT, ICONINFO, IDC_ARROW, LoadCursorW,
    SetCursor,
};

/// A host cursor for this window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCursorImage {
    /// The host hides its pointer: nothing is drawn over the image.
    Hidden,
    /// An image in device pixels, straight (not premultiplied) RGBA, rows
    /// top to bottom.
    Image {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Hotspot in pixels from the top left.
        hotspot: (u32, u32),
        /// `width * height * 4` bytes.
        rgba: Vec<u8>,
    },
}

impl HostCursorImage {
    /// What the shared cursor state shows, sized `scale` device pixels per
    /// stream pixel (the shared `CursorImage::present`, as the macOS Deck's
    /// sharp cursor); `None` for the system arrow (no record yet, or a named
    /// shape).
    #[must_use]
    pub fn from_deck_cursor(cursor: &DeckCursor, scale: f32) -> Option<Self> {
        match cursor {
            DeckCursor::Default | DeckCursor::Named(_) => None,
            DeckCursor::Hidden => Some(Self::Hidden),
            DeckCursor::Image { image, .. } => {
                let presented = image.present(scale);
                // A cursor's colour bitmap is straight alpha; `present` is
                // premultiplied.
                let rgba = presented
                    .rgba_premultiplied
                    .chunks_exact(4)
                    .flat_map(|pixel| {
                        let alpha = pixel[3];
                        let straight = |value: u8| {
                            if alpha == 0 {
                                0
                            } else {
                                let value = (u32::from(value) * 255 + u32::from(alpha) / 2)
                                    / u32::from(alpha);
                                u8::try_from(value).unwrap_or(u8::MAX)
                            }
                        };
                        [
                            straight(pixel[0]),
                            straight(pixel[1]),
                            straight(pixel[2]),
                            alpha,
                        ]
                    })
                    .collect();
                Some(Self::Image {
                    width: u32::from(presented.width),
                    height: u32::from(presented.height),
                    hotspot: (
                        u32::from(presented.hotspot.0),
                        u32::from(presented.hotspot.1),
                    ),
                    rgba,
                })
            }
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Cursor {
    shown: Shown,
}

#[derive(Debug, Default)]
enum Shown {
    #[default]
    Arrow,
    Hidden,
    Image(HCURSOR),
}

impl Cursor {
    /// The cursor to show from now on; the one it replaces is destroyed when
    /// the returned value drops (after the caller has shown the new one).
    #[must_use]
    pub(super) fn set(&mut self, cursor: Option<HostCursorImage>) -> Retired {
        let next = match cursor {
            None => Shown::Arrow,
            Some(HostCursorImage::Hidden) => Shown::Hidden,
            Some(HostCursorImage::Image {
                width,
                height,
                hotspot,
                rgba,
            }) => build(width, height, hotspot, &rgba).map_or(Shown::Arrow, Shown::Image),
        };
        match std::mem::replace(&mut self.shown, next) {
            Shown::Image(old) => Retired(Some(old)),
            Shown::Arrow | Shown::Hidden => Retired(None),
        }
    }

    /// Shows the current cursor at once (a new shape or size while the
    /// mouse rests over the image; Windows asks again only when it moves).
    pub(super) fn apply_now(&self, host_leads: bool) {
        let cursor = match self.shown {
            _ if host_leads => HCURSOR::default(),
            // SAFETY: the system's shared arrow.
            Shown::Arrow => unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            Shown::Hidden => HCURSOR::default(),
            Shown::Image(cursor) => cursor,
        };
        // SAFETY: a cursor this module owns, the system arrow, or none.
        unsafe { SetCursor(cursor) };
    }

    /// `WM_SETCURSOR`: the host's cursor over the image.
    pub(super) fn set_cursor(
        &self,
        _hwnd: HWND,
        lparam: LPARAM,
        over_image: bool,
        host_leads: bool,
    ) -> Option<LRESULT> {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the low word is the hit test"
        )]
        let hit = (lparam.0 & 0xFFFF) as u16;
        if u32::from(hit) != HTCLIENT || !over_image {
            return None;
        }
        let cursor = match self.shown {
            // The host leads: its mark is drawn in the video instead.
            _ if host_leads => HCURSOR::default(),
            Shown::Arrow => return None,
            Shown::Hidden => HCURSOR::default(),
            Shown::Image(cursor) => cursor,
        };
        // SAFETY: a cursor this module owns, or none.
        unsafe { SetCursor(cursor) };
        Some(LRESULT(1))
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        drop(self.set(None));
    }
}

/// A cursor no longer shown, destroyed on drop.
#[derive(Debug)]
pub(super) struct Retired(Option<HCURSOR>);

impl Drop for Retired {
    fn drop(&mut self) {
        if let Some(old) = self.0.take() {
            // SAFETY: a cursor this module created and no longer shows.
            let _ = unsafe { DestroyCursor(old) };
        }
    }
}

fn build(width: u32, height: u32, hotspot: (u32, u32), rgba: &[u8]) -> Option<HCURSOR> {
    let pixels = usize::try_from(width).ok()? * usize::try_from(height).ok()?;
    if pixels == 0 || rgba.len() != pixels * 4 {
        return None;
    }
    let header = BITMAPINFOHEADER {
        biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).ok()?,
        biWidth: i32::try_from(width).ok()?,
        // Negative: rows top to bottom.
        biHeight: -i32::try_from(height).ok()?,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..BITMAPINFOHEADER::default()
    };
    let info = BITMAPINFO {
        bmiHeader: header,
        ..BITMAPINFO::default()
    };
    let mut bits = std::ptr::null_mut();
    // SAFETY: a 32-bit top-down DIB of `pixels` pixels; the colour and
    // mask bitmaps are deleted after the icon copies them.
    unsafe {
        let colour: HBITMAP = CreateDIBSection(
            None,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
        .ok()?;
        if bits.is_null() {
            let _ = DeleteObject(colour);
            return None;
        }
        let target = std::slice::from_raw_parts_mut(bits.cast::<u8>(), pixels * 4);
        for (out, pixel) in target.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            out.copy_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        let mask = CreateBitmap(
            i32::try_from(width).ok()?,
            i32::try_from(height).ok()?,
            1,
            1,
            None,
        );
        let icon = ICONINFO {
            fIcon: false.into(),
            xHotspot: hotspot.0.min(width.saturating_sub(1)),
            yHotspot: hotspot.1.min(height.saturating_sub(1)),
            hbmMask: mask,
            hbmColor: colour,
        };
        let cursor = CreateIconIndirect(&raw const icon)
            .ok()
            .map(|icon| HCURSOR(icon.0));
        let _ = DeleteObject(mask);
        let _ = DeleteObject(colour);
        cursor
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use arcen_media::cursor::CursorImage;
    use arcen_protocol::cursor::CursorImageId;

    #[test]
    fn a_host_image_becomes_a_straight_alpha_cursor_at_the_window_scale() {
        // Half-transparent red, 2×2, hotspot at the bottom right.
        let image =
            CursorImage::from_straight_rgba(2, 2, (1, 1), [200, 0, 0, 128].repeat(4)).unwrap();
        let cursor = DeckCursor::Image {
            id: CursorImageId::new(7).unwrap(),
            image,
            output: 0,
            generation: 1,
        };
        let Some(HostCursorImage::Image {
            width,
            height,
            hotspot,
            rgba,
        }) = HostCursorImage::from_deck_cursor(&cursor, 2.0)
        else {
            panic!("an image");
        };
        assert_eq!((width, height), (4, 4));
        assert_eq!(hotspot, (2, 2));
        let red = rgba[0];
        assert!(
            (198..=202).contains(&red),
            "straight, not premultiplied: {red}"
        );
        assert_eq!(rgba[3], 128);
        assert_eq!(
            HostCursorImage::from_deck_cursor(&DeckCursor::Hidden, 1.0),
            Some(HostCursorImage::Hidden)
        );
        assert_eq!(
            HostCursorImage::from_deck_cursor(&DeckCursor::Default, 1.0),
            None
        );
    }
}
