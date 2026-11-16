// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Small widgets the shared screens draw with: the Settings radio rows and
//! the Settings sidebar.

use std::time::Duration;

/// A radio option with a title (plus optional accent badge) and a wrapped,
/// indented description — the row layout used across the Advanced panel.
pub fn settings_radio<T: PartialEq>(
    ui: &mut egui::Ui,
    current: &mut T,
    value: T,
    badge: Option<&str>,
    title: &str,
    description: &str,
) {
    settings_radio_with_trailing(ui, current, value, badge, title, description, |_| {});
}

/// `settings_radio` with a right-aligned control on the option's own row.
///
/// Used where an option depends on something the user may still have to set
/// up: the status belongs beside the choice it gates, not in a block below it
/// that only appears once the option is already broken.
pub fn settings_radio_with_trailing<T: PartialEq>(
    ui: &mut egui::Ui,
    current: &mut T,
    value: T,
    badge: Option<&str>,
    title: &str,
    description: &str,
    trailing: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.radio_value(
            current,
            value,
            egui::RichText::new(title)
                .color(egui::Color32::BLACK)
                .size(16.0),
        );
        if let Some(badge) = badge {
            ui.label(
                // Muted copper: secondary to the option label, but in the same
                // family as the rest of the palette.
                egui::RichText::new(badge)
                    .color(egui::Color32::from_rgb(0xB0, 0x8A, 0x6E))
                    .size(15.0),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing);
    });
    ui.indent(title, |ui| {
        ui.label(egui::RichText::new(description).color(egui::Color32::DARK_GRAY));
    });
    ui.add_space(12.0);
}

/// A section heading in the Settings sidebar ("GENERAL", "SYSTEM").
pub fn settings_nav_section_label(ui: &mut egui::Ui, label: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(label)
                .size(10.0)
                .strong()
                .color(egui::Color32::from_rgb(0x8B, 0x8B, 0x9A)),
        );
    });
    ui.add_space(4.0);
}

/// One left-nav item in the Settings sidebar. Active items get a light accent
/// highlight pill and accent text (matching the reference design); the label is
/// painted directly so we control the vertical padding precisely. Returns true
/// when clicked.
pub fn settings_nav_item(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    accent: egui::Color32,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(202.0, 42.0), egui::Sense::click());
    let bg = if active {
        egui::Color32::from_rgb(0xE5, 0xDE, 0xD2)
    } else if response.hovered() {
        egui::Color32::from_rgb(0xF0, 0xEC, 0xE4)
    } else {
        egui::Color32::TRANSPARENT
    };
    if bg != egui::Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, 6.0, bg);
    }
    let color = if active {
        accent
    } else {
        egui::Color32::from_rgb(0x1A, 0x1A, 0x1A)
    };
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), egui::FontId::proportional(16.0), color);
    let pos = egui::pos2(rect.left() + 16.0, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(pos, galley, color);
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

/// A "back" control whose left chevron is hand-painted, because egui's bundled
/// fonts render the "←" glyph as a tofu box. Draws `‹ label` in the accent colour and reports
/// clicks like a normal button.
pub fn back_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let color_normal = super::theme::TEXT_SECONDARY;
    let color_hover = super::theme::TEXT_PRIMARY;
    let font = egui::FontId::proportional(17.0);
    let chevron_w = 9.0;
    let gap = 9.0;
    let pad = egui::vec2(2.0, 6.0);

    let text_size = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), color_normal)
        .size();
    let size = egui::vec2(
        pad.x * 2.0 + chevron_w + gap + text_size.x,
        pad.y * 2.0 + text_size.y.max(18.0),
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let color = if response.hovered() {
        color_hover
    } else {
        color_normal
    };

    // Left-pointing chevron.
    let cx = rect.left() + pad.x + chevron_w;
    let cy = rect.center().y;
    let half = 5.0;
    let stroke = egui::Stroke::new(2.0, color);
    ui.painter().line_segment(
        [
            egui::pos2(cx, cy - half),
            egui::pos2(rect.left() + pad.x, cy),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + pad.x, cy),
            egui::pos2(cx, cy + half),
        ],
        stroke,
    );

    let text_pos = egui::pos2(
        rect.left() + pad.x + chevron_w + gap,
        rect.center().y - text_size.y * 0.5,
    );
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
    ui.painter().galley(text_pos, galley, color);

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A labelled single-line field with an accent underline (sign-in, add connection).
pub fn labelled_line_edit(ui: &mut egui::Ui, label: &str, value: &mut String, password: bool) {
    let width = ui.available_width();
    ui.label(
        egui::RichText::new(label)
            .color(egui::Color32::from_rgb(0x4A, 0x4A, 0x4A))
            .strong(),
    );
    let edit = egui::TextEdit::singleline(value)
        .desired_width(width)
        .vertical_align(egui::Align::Center)
        .font(egui::TextStyle::Heading)
        .password(password)
        .frame(egui::Frame::NONE);
    ui.scope(|ui| {
        ui.style_mut().spacing.interact_size.y = 34.0;
        ui.add_sized([width, 34.0], edit);
    });
    let y = ui.cursor().min.y;
    let rect = egui::Rect::from_min_size(egui::pos2(ui.cursor().min.x, y), egui::vec2(width, 1.0));
    ui.painter().rect_filled(rect, 0.0, super::theme::ACCENT);
}

/// How often the connecting arc moves.
pub const CONNECTING_PROGRESS_REPAINT_INTERVAL: Duration = Duration::from_millis(100);

/// Start angle of the connecting arc, quantised to the repaint cadence so the
/// animation never asks for more passes than `CONNECTING_PROGRESS_REPAINT_INTERVAL`.
#[must_use]
pub fn connecting_progress_angle(time_seconds: f64) -> f64 {
    let step = CONNECTING_PROGRESS_REPAINT_INTERVAL.as_secs_f64();
    (time_seconds / step).floor() * step * std::f64::consts::TAU
}

/// Paints the same arc as `egui::Spinner` without its per-frame
/// `request_repaint`, which otherwise drives the UI at display refresh while
/// a session is only connecting.
#[allow(clippy::cast_possible_truncation, reason = "a point on a 24-pixel arc")]
pub fn paint_connecting_progress(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.ctx()
        .request_repaint_after(CONNECTING_PROGRESS_REPAINT_INTERVAL);
    let radius = rect.height().min(rect.width()) / 2.0 - 2.0;
    let start = connecting_progress_angle(ui.input(|input| input.time));
    let end = start + 240f64.to_radians();
    let points = (0..32)
        .map(|i| {
            let angle = start + (end - start) * f64::from(i) / 32.0;
            let (sin, cos) = angle.sin_cos();
            rect.center() + radius * egui::vec2(cos as f32, sin as f32)
        })
        .collect();
    ui.painter().add(egui::Shape::line(
        points,
        egui::Stroke::new(3.0, egui::Color32::from_rgb(0x66, 0x66, 0x66)),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "the angle is quantised, so equal means the same step"
    )]
    fn connecting_progress_animation_is_capped_at_ten_hz() {
        assert!(CONNECTING_PROGRESS_REPAINT_INTERVAL >= Duration::from_millis(100));
        let step = CONNECTING_PROGRESS_REPAINT_INTERVAL.as_secs_f64();
        assert_eq!(
            connecting_progress_angle(0.0),
            connecting_progress_angle(step * 0.99)
        );
        assert!(connecting_progress_angle(step) > connecting_progress_angle(0.0));
    }
}
