// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Small widgets the shared screens draw with: fields, the primary and
//! secondary buttons, the quiet text button, the focus ring and the
//! Settings sidebar.

use std::time::Duration;

/// A section heading in the Settings sidebar ("General", "System"): a
/// sentence-case label in the secondary colour, untracked.
pub fn settings_nav_section_label(ui: &mut egui::Ui, label: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        let colour = super::theme::colours(ui).text2;
        ui.label(
            egui::RichText::new(label)
                .size(super::theme::TEXT_LABEL)
                .color(colour),
        );
    });
    ui.add_space(4.0);
}

/// A field's label: sentence case, the secondary colour, the label size,
/// above its input.
pub fn field_label(ui: &mut egui::Ui, label: &str) {
    let colour = super::theme::colours(ui).text2;
    ui.label(
        egui::RichText::new(label)
            .size(super::theme::TEXT_LABEL)
            .color(colour),
    );
    ui.add_space(super::theme::LABEL_TO_FIELD);
}

/// A single-line input with a constant 1-point border; focus adds a
/// 2-point accent outline outside it at once (the border never changes).
pub fn text_field(
    ui: &mut egui::Ui,
    value: &mut String,
    password: bool,
    width: f32,
    hint: &str,
) -> egui::Response {
    use super::theme;
    let colours = theme::colours(ui);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, theme::CONTROL_HEIGHT),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, theme::CONTROL_RADIUS, colours.field);
    let response = ui.put(
        rect,
        egui::TextEdit::singleline(value)
            // Our own border and ring, painted around it; no egui frame.
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(8, 4)))
            .vertical_align(egui::Align::Center)
            .password(password)
            .hint_text(hint)
            .font(egui::FontId::proportional(theme::TEXT_BODY))
            .text_color(colours.text)
            .desired_width(width),
    );
    field_outline(ui, rect, response.has_focus());
    response
}

/// A control's 1-point border and, when focused, the 2-point accent ring
/// outside it.
pub fn field_outline(ui: &egui::Ui, rect: egui::Rect, focused: bool) {
    use super::theme;
    ui.painter().rect_stroke(
        rect,
        theme::CONTROL_RADIUS,
        egui::Stroke::new(1.0, theme::colours(ui).field_border),
        egui::StrokeKind::Inside,
    );
    if focused {
        focus_ring(ui, rect);
    }
}

/// The primary button's look in every state: [`super::theme::PRIMARY_FILL`],
/// darker under the pointer and darker still pressed; its white text is
/// the same in every state.
pub fn primary_visuals(visuals: &mut egui::Visuals) {
    use super::theme;
    let colours = theme::Palette::of(visuals.dark_mode);
    let widgets = &mut visuals.widgets;
    for (state, fill) in [
        (&mut widgets.inactive, colours.primary),
        (&mut widgets.hovered, colours.primary_hover),
        (&mut widgets.active, colours.primary_pressed),
    ] {
        state.weak_bg_fill = fill;
        state.bg_fill = fill;
        state.bg_stroke = egui::Stroke::NONE;
        state.fg_stroke.color = colours.on_primary;
        state.expansion = 0.0;
        state.corner_radius = egui::CornerRadius::from(theme::CONTROL_RADIUS);
    }
}

/// The secondary look in every state: the field's fill and border, the
/// hover tint under the pointer and the pressed tint pressed; its text is
/// the same in every state.
pub fn secondary_visuals(visuals: &mut egui::Visuals) {
    use super::theme;
    let colours = theme::Palette::of(visuals.dark_mode);
    let widgets = &mut visuals.widgets;
    for (state, fill) in [
        (&mut widgets.inactive, colours.field),
        (&mut widgets.hovered, colours.hover),
        (&mut widgets.active, colours.pressed),
        (&mut widgets.open, colours.hover),
    ] {
        state.weak_bg_fill = fill;
        state.bg_fill = fill;
        state.bg_stroke = egui::Stroke::new(1.0, colours.field_border);
        state.fg_stroke.color = colours.text;
        state.expansion = 0.0;
        state.corner_radius = egui::CornerRadius::from(theme::CONTROL_RADIUS);
    }
}

/// How wide buttons side by side are: the widest of `labels` at the button
/// size, plus [`super::theme::BUTTON_PADDING_X`] on each side, so no label
/// touches its button's edge.
#[must_use]
pub fn button_width(ui: &egui::Ui, labels: &[&str]) -> f32 {
    use super::theme;
    // Measured at the primary's (larger) size, so either kind fits.
    let font = egui::FontId::proportional(theme::PRIMARY_TEXT);
    let widest = labels
        .iter()
        .map(|label| {
            ui.ctx().fonts_mut(|fonts| {
                fonts
                    .layout_no_wrap(
                        (*label).to_owned(),
                        font.clone(),
                        egui::Color32::PLACEHOLDER,
                    )
                    .size()
                    .x
            })
        })
        .fold(0.0_f32, f32::max);
    widest + 2.0 * theme::BUTTON_PADDING_X
}

/// The one filled button on a scene (Connect, a form's Save):
/// [`primary_visuals`], [`super::theme::PRIMARY_HEIGHT`] tall, `width`
/// wide. Never disabled: a click on an action that cannot go ahead yet
/// says why, under the field to fix, and puts the focus there.
pub fn primary_button(ui: &mut egui::Ui, label: &str, width: f32) -> egui::Response {
    use super::theme;
    let response = ui
        .scope(|ui| {
            let on_primary = theme::colours(ui).on_primary;
            primary_visuals(ui.visuals_mut());
            ui.add_sized(
                [width, theme::PRIMARY_HEIGHT],
                egui::Button::new(
                    egui::RichText::new(label)
                        .color(on_primary)
                        .size(theme::PRIMARY_TEXT),
                ),
            )
        })
        .inner;
    if response.has_focus() {
        focus_ring(ui, response.rect);
    }
    response
}

/// Every other button (Edit, Cancel, Import…): [`secondary_visuals`], as
/// tall as the primary, with the same radius and text, `width` wide.
pub fn secondary_button(ui: &mut egui::Ui, label: &str, width: f32) -> egui::Response {
    use super::theme;
    let response = ui
        .scope(|ui| {
            let text = theme::colours(ui).text;
            secondary_visuals(ui.visuals_mut());
            ui.add_sized(
                [width, theme::CONTROL_HEIGHT],
                egui::Button::new(
                    egui::RichText::new(label)
                        .color(text)
                        .size(theme::BUTTON_TEXT),
                ),
            )
        })
        .inner;
    if response.has_focus() {
        focus_ring(ui, response.rect);
    }
    response
}

/// A secondary button as wide as its label needs, greyed out unless
/// `enabled` (an action already under way).
pub fn fitted_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    let width = button_width(ui, &[label]);
    ui.add_enabled_ui(enabled, |ui| secondary_button(ui, label, width))
        .inner
}

/// The space a [`text_button`] leaves either side of its words.
pub const TEXT_BUTTON_INSET: f32 = 4.0;

/// A quiet action beside the buttons (Test connection): text in the
/// secondary ink, no box, underlined under the pointer, as tall as the
/// buttons so it lines up with them. `busy` keeps it from starting again.
pub fn text_button(ui: &mut egui::Ui, label: &str, busy: bool) -> egui::Response {
    use super::theme;
    let colours = theme::colours(ui);
    let font = egui::FontId::proportional(theme::BUTTON_TEXT);
    let galley = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout_no_wrap(label.to_owned(), font, colours.text2));
    let sense = if busy {
        egui::Sense::hover()
    } else {
        egui::Sense::click()
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(
            galley.size().x + 2.0 * TEXT_BUTTON_INSET,
            theme::CONTROL_HEIGHT,
        ),
        sense,
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, !busy, label));
    if ui.is_rect_visible(rect) {
        let at = egui::pos2(
            rect.left() + TEXT_BUTTON_INSET,
            rect.center().y - galley.size().y / 2.0,
        );
        let underline = response.hovered() && !busy;
        let width = galley.size().x;
        let ink = if underline {
            colours.text
        } else {
            colours.text2
        };
        ui.painter().galley(at, galley, ink);
        if underline {
            let y = rect.center().y + theme::BUTTON_TEXT * 0.55;
            ui.painter().line_segment(
                [egui::pos2(at.x, y), egui::pos2(at.x + width, y)],
                egui::Stroke::new(1.0, ink),
            );
        }
        if response.has_focus() {
            focus_ring(ui, rect);
        }
    }
    if response.hovered() && !busy {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// The 2-point accent focus ring outside a control.
pub fn focus_ring(ui: &egui::Ui, rect: egui::Rect) {
    use super::theme;
    ui.painter().rect_stroke(
        rect,
        theme::CONTROL_RADIUS + 1.0,
        egui::Stroke::new(theme::FOCUS_RING_WIDTH, theme::colours(ui).accent),
        egui::StrokeKind::Outside,
    );
}

/// One left-nav item in the Settings sidebar: the active one on the
/// selection tint with the accent bar at its left edge, as Home's selected
/// row; the label painted directly so the vertical padding is exact.
/// Returns true when clicked.
pub fn settings_nav_item(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    let colours = super::theme::colours(ui);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(202.0, 42.0), egui::Sense::click());
    let bg = if active {
        Some(colours.selected)
    } else if response.hovered() {
        Some(colours.hover)
    } else {
        None
    };
    if let Some(bg) = bg {
        ui.painter().rect_filled(rect, 6.0, bg);
    }
    if active {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height())),
            0.0,
            colours.accent,
        );
    }
    let color = colours.text;
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
        egui::Stroke::new(3.0, super::theme::colours(ui).text2),
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
