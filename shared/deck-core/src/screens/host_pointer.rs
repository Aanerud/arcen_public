// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The host-led cursor over the video: while the host, not the Deck's own
//! mouse, moved the pointer last (a Native-tablet pen, an application
//! warping it), the Deck draws the host's cursor image, a plain arrow, or a
//! pen mark at the host's position (`arcen_input::host_pointer` decides
//! which).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "screen geometry in egui points"
)]

use arcen_input::cursor::DeckCursor;
use arcen_input::host_pointer::LedPointerMark;
use arcen_protocol::cursor::CursorImageId;

/// The uploaded host cursor image, kept while its id stays the same.
pub type HostPointerTexture = Option<(CursorImageId, egui::TextureHandle)>;

/// The viewer point for a position at `fraction` of the stream, or `None`
/// outside it.
#[must_use]
pub fn host_pointer_point(image_rect: egui::Rect, fraction: (f64, f64)) -> Option<egui::Pos2> {
    let (x, y) = fraction;
    if !(0.0..1.0).contains(&x) || !(0.0..1.0).contains(&y) {
        return None;
    }
    Some(egui::pos2(
        image_rect.left() + (x as f32) * image_rect.width(),
        image_rect.top() + (y as f32) * image_rect.height(),
    ))
}

/// A plain arrow with its tip at `at`, for a host cursor the Deck has no
/// image of.
fn paint_pointer_arrow(painter: &egui::Painter, at: egui::Pos2) {
    const ARROW: [[f32; 2]; 7] = [
        [0.0, 0.0],
        [0.0, 17.0],
        [4.0, 13.0],
        [7.0, 20.0],
        [9.5, 19.0],
        [6.5, 12.0],
        [12.0, 12.0],
    ];
    let points = ARROW
        .iter()
        .map(|[x, y]| at + egui::vec2(*x, *y))
        .collect::<Vec<_>>();
    painter.add(egui::Shape::convex_polygon(
        points.clone(),
        egui::Color32::WHITE,
        egui::Stroke::NONE,
    ));
    painter.add(egui::Shape::closed_line(
        points,
        egui::Stroke::new(1.0, egui::Color32::BLACK),
    ));
}

/// A small pen mark centred on `at`: a white dot ringed in black, visible
/// on any background, for a host that hides its pointer while a bridged pen
/// drives it ([`LedPointerMark::PenMark`]).
fn paint_pen_mark(painter: &egui::Painter, at: egui::Pos2) {
    painter.circle(
        at,
        3.5,
        egui::Color32::WHITE,
        egui::Stroke::new(1.5, egui::Color32::BLACK),
    );
}

/// Draws `mark` at `at` within `image_rect`: for an image, `shown`'s image
/// scaled as the video shows a stream `stream_width` pixels wide, uploaded
/// once per image id into `cache`.
pub fn paint_host_pointer(
    ui: &egui::Ui,
    image_rect: egui::Rect,
    at: egui::Pos2,
    mark: LedPointerMark,
    shown: &DeckCursor,
    stream_width: Option<usize>,
    cache: &mut HostPointerTexture,
) {
    let painter = ui.painter().with_clip_rect(image_rect);
    match mark {
        LedPointerMark::Nothing => return,
        LedPointerMark::PenMark => return paint_pen_mark(&painter, at),
        LedPointerMark::Arrow => return paint_pointer_arrow(&painter, at),
        LedPointerMark::Image => {}
    }
    let DeckCursor::Image { id, image, .. } = shown else {
        return;
    };
    let Some(stream_width) = stream_width else {
        return;
    };
    if stream_width == 0 {
        return;
    }
    let texture = match &*cache {
        Some((cached, texture)) if cached == id => texture.clone(),
        _ => {
            let size = [image.width() as usize, image.height() as usize];
            let texture = ui.ctx().load_texture(
                "host-pointer",
                egui::ColorImage::from_rgba_unmultiplied(size, image.rgba()),
                egui::TextureOptions::LINEAR,
            );
            *cache = Some((*id, texture.clone()));
            texture
        }
    };
    let points_per_pixel = image_rect.width() / stream_width as f32;
    let (hotspot_x, hotspot_y) = image.hotspot();
    let min = at - egui::vec2(hotspot_x as f32, hotspot_y as f32) * points_per_pixel;
    let size = egui::vec2(image.width() as f32, image.height() as f32) * points_per_pixel;
    painter.image(
        texture.id(),
        egui::Rect::from_min_size(min, size),
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}
