//! Cursor metadata (`SPA_META_Cursor`) as plain data.
//!
//! Mutter (GNOME 40 and 49) writes one of four things into a buffer's
//! cursor metadata:
//!
//! - cursor id 0: the cursor is not on this output, or the pointer is not
//!   visible. No other field is written.
//! - id 1 and `bitmap_offset` 0: position only; the sprite is unchanged.
//! - id 1 and a bitmap header that is all zero: an empty sprite (a client
//!   set no cursor, or a blank one). gnome-remote-desktop reads this as
//!   "hidden", and so does capenc.
//! - id 1 and a bitmap: a new sprite, premultiplied `SPA_VIDEO_FORMAT_RGBA`
//!   in stream pixels.
//!
//! The bitmap is written only when the sprite changes (and when the stream
//! source starts), and a cursor-only update is a buffer whose chunks are
//! empty. The `PipeWire` callback copies the meta bytes into [`CursorMeta`]
//! (the bitmap only when present) and folds it into [`CursorState`]; nothing
//! here touches `PipeWire`.

use std::sync::Arc;

use arcen_media::cursor::CursorImage;

use super::format::{CURSOR_BITMAP_MAX_SIDE, SPA_META_BITMAP_BYTES, SPA_META_CURSOR_BYTES};

/// `SPA_VIDEO_FORMAT_RGBx`.
pub const SPA_VIDEO_FORMAT_RGBX: u32 = 7;
/// `SPA_VIDEO_FORMAT_BGRx`.
pub const SPA_VIDEO_FORMAT_BGRX: u32 = 8;
/// `SPA_VIDEO_FORMAT_RGBA` (Mutter's cursor bitmaps).
pub const SPA_VIDEO_FORMAT_RGBA: u32 = 11;
/// `SPA_VIDEO_FORMAT_BGRA`.
pub const SPA_VIDEO_FORMAT_BGRA: u32 = 12;
/// `SPA_VIDEO_FORMAT_ARGB`.
pub const SPA_VIDEO_FORMAT_ARGB: u32 = 13;
/// `SPA_VIDEO_FORMAT_ABGR`.
pub const SPA_VIDEO_FORMAT_ABGR: u32 = 14;

/// A cursor bitmap copied out of the producer's buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorBitmap {
    /// SPA video format id of the bitmap (ARGB-family on Mutter).
    pub spa_format: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Tightly packed rows, four bytes per pixel.
    pub pixels: Vec<u8>,
}

impl CursorBitmap {
    /// The bitmap as a straight-alpha image with `hotspot`. Pixels are
    /// premultiplied, as Mutter reads them from its cursor texture. `None`
    /// for a format capenc does not know.
    #[must_use]
    pub fn to_image(&self, hotspot: (i32, i32)) -> Option<CursorImage> {
        let order: [usize; 4] = match self.spa_format {
            SPA_VIDEO_FORMAT_RGBA | SPA_VIDEO_FORMAT_RGBX => [0, 1, 2, 3],
            SPA_VIDEO_FORMAT_BGRA | SPA_VIDEO_FORMAT_BGRX => [2, 1, 0, 3],
            SPA_VIDEO_FORMAT_ARGB => [1, 2, 3, 0],
            SPA_VIDEO_FORMAT_ABGR => [3, 2, 1, 0],
            _ => return None,
        };
        let opaque = matches!(
            self.spa_format,
            SPA_VIDEO_FORMAT_RGBX | SPA_VIDEO_FORMAT_BGRX
        );
        let mut rgba = Vec::with_capacity(self.pixels.len());
        for pixel in self.pixels.chunks_exact(4) {
            rgba.extend_from_slice(&[
                pixel[order[0]],
                pixel[order[1]],
                pixel[order[2]],
                if opaque { u8::MAX } else { pixel[order[3]] },
            ]);
        }
        CursorImage::from_premultiplied_rgba(
            self.width,
            self.height,
            (i64::from(hotspot.0), i64::from(hotspot.1)),
            &rgba,
        )
        .ok()
    }
}

/// What a buffer says about the cursor sprite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorSprite {
    /// No new sprite: keep the last one.
    Unchanged,
    /// An empty sprite: the desktop hides the pointer.
    Empty,
    /// A new sprite.
    Bitmap(CursorBitmap),
}

/// One buffer's cursor metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorMeta {
    /// Producer cursor id; 0 means no cursor on this output.
    pub id: u32,
    /// Position of the hotspot within the output.
    pub position: (i32, i32),
    /// Hotspot within the bitmap.
    pub hotspot: (i32, i32),
    /// The sprite this buffer reports.
    pub sprite: CursorSprite,
}

/// Why cursor metadata was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMetaError {
    /// The meta is shorter than its fixed header.
    Truncated,
    /// The bitmap header or pixels fall outside the meta.
    BitmapOutOfBounds,
    /// The bitmap is larger than capenc negotiated or has a short stride.
    BitmapGeometry,
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_ne_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn read_i32(bytes: &[u8], at: usize) -> Option<i32> {
    let end = at.checked_add(4)?;
    Some(i32::from_ne_bytes(bytes.get(at..end)?.try_into().ok()?))
}

/// Parses `struct spa_meta_cursor` and its optional `spa_meta_bitmap`.
///
/// Layout (native endian): `id u32, flags u32, position {i32, i32}, hotspot
/// {i32, i32}, bitmap_offset u32`; a nonzero `bitmap_offset` points at
/// `format u32, size {u32, u32}, stride i32, offset u32` whose `offset` is
/// relative to the bitmap header. A bitmap header with zero size, format or
/// data offset is Mutter's empty sprite.
///
/// # Errors
///
/// [`CursorMetaError`] for truncated or out-of-bounds metadata.
pub fn parse_cursor_meta(bytes: &[u8]) -> Result<CursorMeta, CursorMetaError> {
    if bytes.len() < SPA_META_CURSOR_BYTES {
        return Err(CursorMetaError::Truncated);
    }
    let field = |at| read_i32(bytes, at).ok_or(CursorMetaError::Truncated);
    let id = read_u32(bytes, 0).ok_or(CursorMetaError::Truncated)?;
    let position = (field(8)?, field(12)?);
    let hotspot = (field(16)?, field(20)?);
    let bitmap_offset = read_u32(bytes, 24).ok_or(CursorMetaError::Truncated)? as usize;
    let sprite = if id == 0 || bitmap_offset == 0 {
        CursorSprite::Unchanged
    } else {
        parse_bitmap(bytes, bitmap_offset)?
    };
    Ok(CursorMeta {
        id,
        position,
        hotspot,
        sprite,
    })
}

fn parse_bitmap(bytes: &[u8], header: usize) -> Result<CursorSprite, CursorMetaError> {
    if header < SPA_META_CURSOR_BYTES
        || header
            .checked_add(SPA_META_BITMAP_BYTES)
            .is_none_or(|end| end > bytes.len())
    {
        return Err(CursorMetaError::BitmapOutOfBounds);
    }
    let read = |at: usize| read_u32(bytes, header + at).ok_or(CursorMetaError::BitmapOutOfBounds);
    let spa_format = read(0)?;
    let width = read(4)?;
    let height = read(8)?;
    let stride = read_i32(bytes, header + 12).ok_or(CursorMetaError::BitmapOutOfBounds)?;
    let pixel_offset = read(16)? as usize;
    if width == 0 || height == 0 || spa_format == 0 || pixel_offset == 0 {
        return Ok(CursorSprite::Empty);
    }
    if width > CURSOR_BITMAP_MAX_SIDE || height > CURSOR_BITMAP_MAX_SIDE {
        return Err(CursorMetaError::BitmapGeometry);
    }
    let row_bytes = width as usize * 4;
    let stride = usize::try_from(stride).map_err(|_| CursorMetaError::BitmapGeometry)?;
    if stride < row_bytes {
        return Err(CursorMetaError::BitmapGeometry);
    }
    let start = header
        .checked_add(pixel_offset)
        .ok_or(CursorMetaError::BitmapOutOfBounds)?;
    let span = stride * (height as usize - 1) + row_bytes;
    let end = start
        .checked_add(span)
        .ok_or(CursorMetaError::BitmapOutOfBounds)?;
    let source = bytes
        .get(start..end)
        .ok_or(CursorMetaError::BitmapOutOfBounds)?;
    let mut pixels = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        pixels.extend_from_slice(&source[row * stride..row * stride + row_bytes]);
    }
    Ok(CursorSprite::Bitmap(CursorBitmap {
        spa_format,
        width,
        height,
        pixels,
    }))
}

/// The latest cursor facts for an output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorState {
    /// Whether any cursor metadata has arrived on this stream.
    pub metadata_seen: bool,
    /// Whether the cursor is on this output.
    pub visible: bool,
    /// Producer cursor id.
    pub id: u32,
    /// Hotspot position within the output.
    pub position: (i32, i32),
    /// Hotspot within the bitmap.
    pub hotspot: (i32, i32),
    /// Increments whenever a new sprite (a bitmap or an empty one) arrives.
    pub bitmap_serial: u64,
    /// The latest bitmap; `None` while the sprite is empty.
    pub bitmap: Option<Arc<CursorBitmap>>,
    /// Whether the latest sprite is empty (the desktop hides the pointer).
    pub empty: bool,
    /// Cursor-only (size-0) buffers seen.
    pub cursor_only_updates: u64,
}

impl CursorState {
    /// Folds one buffer's metadata in. `cursor_only` marks a size-0 buffer.
    pub fn apply(&mut self, meta: CursorMeta, cursor_only: bool) {
        self.metadata_seen = true;
        self.visible = meta.id != 0;
        self.id = meta.id;
        if self.visible {
            self.position = meta.position;
        }
        // Mutter writes a hotspot only with a sprite; a position-only update
        // carries 0,0, which is not the sprite's hotspot.
        match meta.sprite {
            CursorSprite::Unchanged => {}
            CursorSprite::Empty => {
                self.bitmap_serial = self.bitmap_serial.wrapping_add(1);
                self.bitmap = None;
                self.empty = true;
                self.hotspot = (0, 0);
            }
            CursorSprite::Bitmap(bitmap) => {
                self.bitmap_serial = self.bitmap_serial.wrapping_add(1);
                self.bitmap = Some(Arc::new(bitmap));
                self.empty = false;
                self.hotspot = meta.hotspot;
            }
        }
        if cursor_only {
            self.cursor_only_updates = self.cursor_only_updates.saturating_add(1);
        }
    }
}

/// Cursor metadata gathered during one `PipeWire` process call, applied to
/// [`CursorState`] once. GNOME 40 sends the bitmap only when the shape
/// changes, so updates coalesce without loss: latest position and id, latest
/// bitmap kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingCursor {
    meta: Option<CursorMeta>,
    cursor_only: u64,
}

impl PendingCursor {
    /// Records one buffer's metadata. `cursor_only` marks a size-0 buffer.
    pub fn push(&mut self, meta: CursorMeta, cursor_only: bool) {
        self.meta = Some(match self.meta.take() {
            None => meta,
            // The hotspot belongs to the sprite: Mutter writes 0,0 with a
            // position-only update, so a later one keeps the older sprite's.
            Some(older) => match meta.sprite {
                CursorSprite::Unchanged => CursorMeta {
                    sprite: older.sprite,
                    hotspot: older.hotspot,
                    ..meta
                },
                _ => meta,
            },
        });
        if cursor_only {
            self.cursor_only = self.cursor_only.saturating_add(1);
        }
    }

    /// Whether anything waits to be applied.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.meta.is_some()
    }

    /// Applies everything pending to `state`.
    pub fn flush_into(&mut self, state: &mut CursorState) {
        let Some(meta) = self.meta.take() else {
            return;
        };
        let extra_cursor_only = self.cursor_only.saturating_sub(1);
        state.apply(meta, self.cursor_only > 0);
        state.cursor_only_updates = state.cursor_only_updates.saturating_add(extra_cursor_only);
        self.cursor_only = 0;
    }
}

/// Turns the stream's [`CursorState`] into the cursor events capenc sends
/// its parent: one when the cursor enters or leaves the output, or its
/// sprite changes; never for movement alone.
#[derive(Debug, Default)]
pub struct CursorEmitter {
    last: Option<(bool, u64)>,
    unknown_formats: u64,
}

impl CursorEmitter {
    /// The event for `state`, when it changed since the last one.
    pub fn poll(
        &mut self,
        state: &CursorState,
    ) -> Option<arcen_media::capenc_stream::CapencCursorEvent> {
        use arcen_media::capenc_stream::{CapencCursorEvent, CapencCursorSprite};
        if !state.metadata_seen {
            return None;
        }
        let key = (state.visible, state.bitmap_serial);
        if self.last == Some(key) {
            return None;
        }
        let sprite_changed = self
            .last
            .is_none_or(|(_, serial)| serial != state.bitmap_serial);
        self.last = Some(key);
        let sprite = if !sprite_changed {
            CapencCursorSprite::Unchanged
        } else if let Some(bitmap) = state.bitmap.as_deref() {
            match bitmap.to_image(state.hotspot) {
                Some(image) => CapencCursorSprite::Image(image),
                None => {
                    self.unknown_formats += 1;
                    CapencCursorSprite::Unchanged
                }
            }
        } else if state.empty {
            CapencCursorSprite::Empty
        } else {
            CapencCursorSprite::Unchanged
        };
        Some(CapencCursorEvent {
            on_output: state.visible,
            sprite,
        })
    }

    /// Sprites dropped because their pixel format is unknown.
    #[must_use]
    pub const fn unknown_formats(&self) -> u64 {
        self.unknown_formats
    }
}

#[cfg(test)]
pub(crate) fn encode_cursor_meta(
    id: u32,
    position: (i32, i32),
    hotspot: (i32, i32),
    bitmap: Option<(u32, u32, i32)>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    let bitmap_offset = if bitmap.is_some() {
        SPA_META_CURSOR_BYTES as u32
    } else {
        0
    };
    for value in [id, 0] {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    for value in [position.0, position.1, hotspot.0, hotspot.1] {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes.extend_from_slice(&bitmap_offset.to_ne_bytes());
    if let Some((width, height, stride)) = bitmap {
        for value in [7u32, width, height] {
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        bytes.extend_from_slice(&stride.to_ne_bytes());
        bytes.extend_from_slice(&(SPA_META_BITMAP_BYTES as u32).to_ne_bytes());
        let stride = usize::try_from(stride).unwrap_or(0);
        for row in 0..height as usize {
            for column in 0..stride {
                bytes.push(u8::try_from((row * 31 + column) % 251).unwrap());
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::format::cursor_meta_size;

    #[test]
    fn position_only_meta_has_no_bitmap() {
        let bytes = encode_cursor_meta(1, (300, 300), (0, 0), None);
        let meta = parse_cursor_meta(&bytes).unwrap();
        assert_eq!(meta.id, 1);
        assert_eq!(meta.position, (300, 300));
        assert_eq!(meta.sprite, CursorSprite::Unchanged);
    }

    #[test]
    fn bitmap_rows_are_copied_without_padding() {
        let bytes = encode_cursor_meta(1, (10, 20), (3, 4), Some((5, 3, 24)));
        let meta = parse_cursor_meta(&bytes).unwrap();
        let CursorSprite::Bitmap(bitmap) = meta.sprite else {
            panic!("expected a bitmap");
        };
        assert_eq!((bitmap.width, bitmap.height, bitmap.spa_format), (5, 3, 7));
        assert_eq!(bitmap.pixels.len(), 5 * 4 * 3);
        // Row 1 starts at source byte 24, not 20.
        assert_eq!(bitmap.pixels[20], 31);
    }

    #[test]
    fn largest_negotiated_bitmap_fits_the_meta() {
        let bytes = encode_cursor_meta(
            2,
            (0, 0),
            (0, 0),
            Some((
                CURSOR_BITMAP_MAX_SIDE,
                CURSOR_BITMAP_MAX_SIDE,
                i32::try_from(CURSOR_BITMAP_MAX_SIDE * 4).unwrap(),
            )),
        );
        assert_eq!(
            bytes.len(),
            cursor_meta_size(CURSOR_BITMAP_MAX_SIDE, CURSOR_BITMAP_MAX_SIDE)
        );
        assert!(matches!(
            parse_cursor_meta(&bytes).unwrap().sprite,
            CursorSprite::Bitmap(_)
        ));
    }

    #[test]
    fn malformed_cursor_meta_is_refused() {
        assert_eq!(parse_cursor_meta(&[0; 27]), Err(CursorMetaError::Truncated));
        let mut truncated = encode_cursor_meta(1, (0, 0), (0, 0), Some((8, 8, 32)));
        truncated.truncate(truncated.len() - 1);
        assert_eq!(
            parse_cursor_meta(&truncated),
            Err(CursorMetaError::BitmapOutOfBounds)
        );
        let short_stride = encode_cursor_meta(1, (0, 0), (0, 0), Some((8, 2, 16)));
        assert_eq!(
            parse_cursor_meta(&short_stride),
            Err(CursorMetaError::BitmapGeometry)
        );
        let negative = encode_cursor_meta(1, (0, 0), (0, 0), Some((1, 1, -4)));
        assert_eq!(
            parse_cursor_meta(&negative),
            Err(CursorMetaError::BitmapGeometry)
        );
        let oversized = encode_cursor_meta(1, (0, 0), (0, 0), Some((385, 1, 1540)));
        assert_eq!(
            parse_cursor_meta(&oversized),
            Err(CursorMetaError::BitmapGeometry)
        );
    }

    #[test]
    fn pending_cursor_keeps_the_latest_bitmap_and_position() {
        let mut pending = PendingCursor::default();
        let shape =
            parse_cursor_meta(&encode_cursor_meta(1, (5, 6), (1, 1), Some((2, 2, 8)))).unwrap();
        pending.push(shape.clone(), false);
        let moved = parse_cursor_meta(&encode_cursor_meta(1, (9, 9), (1, 1), None)).unwrap();
        pending.push(moved, true);
        pending.push(
            parse_cursor_meta(&encode_cursor_meta(1, (10, 11), (1, 1), None)).unwrap(),
            true,
        );
        assert!(pending.is_pending());
        let mut state = CursorState::default();
        pending.flush_into(&mut state);
        assert!(!pending.is_pending());
        assert_eq!(state.position, (10, 11));
        assert_eq!(state.bitmap_serial, 1);
        assert_eq!(Some(&state.sprite_for_test()), Some(&shape.sprite));
        assert_eq!(state.cursor_only_updates, 2);
        // A newer bitmap replaces the older one.
        let newer =
            parse_cursor_meta(&encode_cursor_meta(2, (0, 0), (0, 0), Some((3, 1, 12)))).unwrap();
        pending.push(newer.clone(), false);
        pending.push(
            parse_cursor_meta(&encode_cursor_meta(2, (4, 4), (0, 0), None)).unwrap(),
            false,
        );
        pending.flush_into(&mut state);
        assert_eq!(Some(&state.sprite_for_test()), Some(&newer.sprite));
        assert_eq!(state.bitmap_serial, 2);
        // Nothing pending changes nothing.
        let before = state.clone();
        pending.flush_into(&mut state);
        assert_eq!(state, before);
    }

    #[test]
    fn state_tracks_visibility_bitmap_serial_and_cursor_only_updates() {
        let mut state = CursorState::default();
        let with_bitmap =
            parse_cursor_meta(&encode_cursor_meta(1, (5, 6), (1, 1), Some((2, 2, 8)))).unwrap();
        state.apply(with_bitmap, false);
        assert!(state.visible && state.metadata_seen);
        assert_eq!(state.bitmap_serial, 1);
        let moved = parse_cursor_meta(&encode_cursor_meta(1, (7, 8), (1, 1), None)).unwrap();
        state.apply(moved, true);
        assert_eq!(state.position, (7, 8));
        assert_eq!(state.bitmap_serial, 1, "position-only keeps the bitmap");
        assert!(state.bitmap.is_some());
        assert_eq!(state.cursor_only_updates, 1);
        let gone = parse_cursor_meta(&encode_cursor_meta(0, (0, 0), (0, 0), None)).unwrap();
        state.apply(gone, true);
        assert!(!state.visible);
        assert_eq!(
            state.position,
            (7, 8),
            "hidden cursor keeps its last position"
        );
    }

    impl CursorState {
        fn sprite_for_test(&self) -> CursorSprite {
            match &self.bitmap {
                Some(bitmap) => CursorSprite::Bitmap((**bitmap).clone()),
                None if self.empty => CursorSprite::Empty,
                None => CursorSprite::Unchanged,
            }
        }
    }

    #[test]
    fn mutters_zeroed_bitmap_is_an_empty_sprite() {
        // GNOME 40 lab: `id=1 pos=300,300 hotspot=0,0 bitmap_offset=28` and
        // an all-zero `spa_meta_bitmap` (`set_empty_cursor_sprite_metadata`).
        let mut bytes = encode_cursor_meta(1, (300, 300), (0, 0), None);
        bytes[24..28].copy_from_slice(&(SPA_META_CURSOR_BYTES as u32).to_ne_bytes());
        bytes.extend_from_slice(&[0; SPA_META_BITMAP_BYTES]);
        let meta = parse_cursor_meta(&bytes).unwrap();
        assert_eq!(meta.sprite, CursorSprite::Empty);
        let mut state = CursorState::default();
        state.apply(
            parse_cursor_meta(&encode_cursor_meta(1, (5, 6), (1, 1), Some((2, 2, 8)))).unwrap(),
            false,
        );
        let serial = state.bitmap_serial;
        state.apply(meta, true);
        assert!(state.empty && state.bitmap.is_none() && state.visible);
        assert_eq!(state.bitmap_serial, serial + 1);
        // A later position-only update keeps the empty sprite.
        state.apply(
            parse_cursor_meta(&encode_cursor_meta(1, (9, 9), (0, 0), None)).unwrap(),
            true,
        );
        assert!(state.empty);
    }

    #[test]
    fn a_later_position_keeps_the_pending_sprites_hotspot() {
        let mut pending = PendingCursor::default();
        pending.push(
            parse_cursor_meta(&encode_cursor_meta(1, (5, 6), (3, 4), Some((8, 8, 32)))).unwrap(),
            false,
        );
        pending.push(
            parse_cursor_meta(&encode_cursor_meta(1, (9, 9), (0, 0), None)).unwrap(),
            true,
        );
        let mut state = CursorState::default();
        pending.flush_into(&mut state);
        assert_eq!(state.position, (9, 9));
        assert_eq!(state.hotspot, (3, 4));
        assert!(state.bitmap.is_some());
    }

    #[test]
    fn pending_empty_sprite_is_not_lost_to_a_later_position() {
        let mut pending = PendingCursor::default();
        let mut bytes = encode_cursor_meta(1, (1, 1), (0, 0), None);
        bytes[24..28].copy_from_slice(&(SPA_META_CURSOR_BYTES as u32).to_ne_bytes());
        bytes.extend_from_slice(&[0; SPA_META_BITMAP_BYTES]);
        pending.push(parse_cursor_meta(&bytes).unwrap(), true);
        pending.push(
            parse_cursor_meta(&encode_cursor_meta(1, (2, 2), (0, 0), None)).unwrap(),
            true,
        );
        let mut state = CursorState::default();
        pending.flush_into(&mut state);
        assert!(state.empty);
        assert_eq!(state.position, (2, 2));
    }

    #[test]
    fn premultiplied_bitmaps_convert_to_images_per_format() {
        let bitmap = |spa_format, pixels: Vec<u8>| CursorBitmap {
            spa_format,
            width: 1,
            height: 1,
            pixels,
        };
        let rgba = bitmap(SPA_VIDEO_FORMAT_RGBA, vec![64, 32, 0, 128])
            .to_image((0, 0))
            .unwrap();
        assert_eq!(rgba.rgba(), &[128, 64, 0, 128]);
        let bgra = bitmap(SPA_VIDEO_FORMAT_BGRA, vec![0, 32, 64, 128])
            .to_image((0, 0))
            .unwrap();
        assert_eq!(bgra.rgba(), &[128, 64, 0, 128]);
        let argb = bitmap(SPA_VIDEO_FORMAT_ARGB, vec![255, 1, 2, 3])
            .to_image((0, 0))
            .unwrap();
        assert_eq!(argb.rgba(), &[1, 2, 3, 255]);
        let rgbx = bitmap(SPA_VIDEO_FORMAT_RGBX, vec![1, 2, 3, 0])
            .to_image((5, 5))
            .unwrap();
        assert_eq!(rgbx.rgba(), &[1, 2, 3, 255]);
        assert_eq!(rgbx.hotspot(), (0, 0), "hotspot clamped into the image");
        assert!(bitmap(2, vec![0; 4]).to_image((0, 0)).is_none());
    }

    #[test]
    fn emitter_reports_changes_but_not_movement() {
        use arcen_media::capenc_stream::CapencCursorSprite;
        let mut emitter = CursorEmitter::default();
        let mut state = CursorState::default();
        assert!(emitter.poll(&state).is_none(), "nothing before metadata");
        let mut rgba = Vec::new();
        for _ in 0..4 {
            rgba.extend_from_slice(&[255, 255, 255, 255]);
        }
        let mut meta =
            parse_cursor_meta(&encode_cursor_meta(1, (5, 6), (1, 1), Some((2, 2, 8)))).unwrap();
        if let CursorSprite::Bitmap(bitmap) = &mut meta.sprite {
            bitmap.spa_format = SPA_VIDEO_FORMAT_RGBA;
            bitmap.pixels.clone_from(&rgba);
        }
        state.apply(meta, false);
        let first = emitter.poll(&state).unwrap();
        assert!(first.on_output);
        let CapencCursorSprite::Image(image) = first.sprite else {
            panic!("expected an image");
        };
        assert_eq!(image.hotspot(), (1, 1));
        // Movement only.
        state.apply(
            parse_cursor_meta(&encode_cursor_meta(1, (50, 60), (0, 0), None)).unwrap(),
            true,
        );
        assert!(emitter.poll(&state).is_none());
        assert_eq!(state.hotspot, (1, 1), "position-only keeps the hotspot");
        // Leaving the output.
        state.apply(
            parse_cursor_meta(&encode_cursor_meta(0, (0, 0), (0, 0), None)).unwrap(),
            true,
        );
        let left = emitter.poll(&state).unwrap();
        assert!(!left.on_output);
        assert_eq!(left.sprite, CapencCursorSprite::Unchanged);
        // An empty sprite.
        let mut bytes = encode_cursor_meta(1, (1, 1), (0, 0), None);
        bytes[24..28].copy_from_slice(&(SPA_META_CURSOR_BYTES as u32).to_ne_bytes());
        bytes.extend_from_slice(&[0; SPA_META_BITMAP_BYTES]);
        state.apply(parse_cursor_meta(&bytes).unwrap(), true);
        assert_eq!(
            emitter.poll(&state).unwrap().sprite,
            CapencCursorSprite::Empty
        );
        assert!(emitter.poll(&state).is_none());
    }
}
