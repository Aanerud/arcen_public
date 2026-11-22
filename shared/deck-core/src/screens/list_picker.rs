// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The inline list picker: one setting chosen from a short list, in place.
//!
//! Its label, the value in force and one help line. Clicking the value
//! opens the choices under it, pushing what follows down (never a
//! floating list); the chosen one is in the accent with a bar at its left
//! edge. A click on a choice takes it and closes the list; Esc, a click
//! elsewhere or Tab away closes it too. Up and Down move, Enter chooses.
//! Every single choice from a short list uses it: the connection form's
//! "On this machine" and the Settings pages.

use super::theme;

/// A choice's row height.
pub const ROW_HEIGHT: f32 = 36.0;
/// The value's text size.
pub const VALUE_TEXT: f32 = 16.0;
/// How long the list takes to open or close.
pub const SLIDE_SECONDS: f32 = 0.12;
/// The chosen choice's bar.
const BAR_WIDTH: f32 = 3.0;
/// The text's inset from the picker's left edge.
const INSET: f32 = 10.0;

/// What the picker keeps between passes (egui memory).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PickerMemory {
    /// The highlighted choice while the list is open.
    highlight: usize,
}

/// The one picker open at a time.
fn open_id() -> egui::Id {
    egui::Id::new("list_picker_open")
}

/// Whether the picker `id` is open.
#[must_use]
pub fn is_open(ctx: &egui::Context, id: egui::Id) -> bool {
    open_picker(ctx) == Some(id)
}

/// Whether any picker is open (Esc and Enter are its keys then). A picker
/// no longer drawn (its page closed while its list was open) is not.
#[must_use]
pub fn any_open(ctx: &egui::Context) -> bool {
    open_picker(ctx).is_some()
}

/// The open picker, while it was drawn on this pass or the last one.
fn open_picker(ctx: &egui::Context) -> Option<egui::Id> {
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|data| data.get_temp::<(egui::Id, u64)>(open_id()))
        .filter(|(_, drawn)| drawn + 1 >= pass)
        .map(|(id, _)| id)
}

/// The picker whose value had the keyboard focus on the last pass.
fn focus_id() -> egui::Id {
    egui::Id::new("list_picker_focus")
}

/// Whether a picker has the keyboard focus (Enter and Space are its keys).
#[must_use]
pub fn has_focus(ctx: &egui::Context) -> bool {
    let picker = ctx.data(|data| data.get_temp::<egui::Id>(focus_id()));
    picker.is_some() && picker == ctx.memory(egui::Memory::focused)
}

fn set_open(ctx: &egui::Context, id: Option<egui::Id>) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|data| match id {
        Some(id) => {
            data.insert_temp(open_id(), (id, pass));
        }
        None => {
            data.remove::<(egui::Id, u64)>(open_id());
        }
    });
}

/// What [`list_picker`] did this pass.
#[derive(Debug, Clone)]
pub struct PickerResponse {
    /// The value row's response (focus, accessibility).
    pub response: egui::Response,
    /// A choice was taken.
    pub changed: bool,
    /// The list is open.
    pub open: bool,
}

/// One setting chosen from `choices` (value and its label), `width` wide:
/// `title` above (none under a heading that names it), the value in force,
/// and `help` under it, wrapped to the width.
pub fn list_picker<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    title: Option<&str>,
    current: &mut T,
    choices: &[(T, String)],
    help: Option<&str>,
    width: f32,
) -> PickerResponse {
    let colours = theme::colours(ui);
    let id = ui.make_persistent_id(("list_picker", id_salt));
    let ctx = ui.ctx().clone();
    let mut open = is_open(&ctx, id);
    let mut changed = false;
    let selected = choices
        .iter()
        .position(|(value, _)| value == current)
        .unwrap_or(0);
    let mut memory = ui
        .data(|data| data.get_temp::<PickerMemory>(id))
        .unwrap_or(PickerMemory {
            highlight: selected,
        });

    if let Some(title) = title {
        ui.add(
            egui::Label::new(
                egui::RichText::new(title)
                    .size(theme::TEXT_LABEL)
                    .color(colours.text2),
            )
            .wrap(),
        );
        ui.add_space(theme::LABEL_TO_FIELD);
    }

    // The value in force: the field's border, its text in the body ink.
    let value_label = choices
        .get(selected)
        .map_or("", |(_, label)| label.as_str());
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, theme::CONTROL_HEIGHT),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::ComboBox,
            true,
            format!("{}, {value_label}", title.unwrap_or("")),
        )
    });
    if ui.is_rect_visible(rect) {
        let fill = if response.is_pointer_button_down_on() {
            colours.pressed
        } else if response.hovered() || open {
            colours.hover
        } else {
            colours.field
        };
        let painter = ui.painter();
        painter.rect_filled(rect, theme::CONTROL_RADIUS, fill);
        painter.rect_stroke(
            rect,
            theme::CONTROL_RADIUS,
            egui::Stroke::new(1.0, colours.field_border),
            egui::StrokeKind::Inside,
        );
        let galley = painter.layout(
            value_label.to_owned(),
            egui::FontId::proportional(VALUE_TEXT),
            colours.text,
            width - 2.0 * INSET,
        );
        painter.galley(
            egui::pos2(rect.left() + INSET, rect.center().y - galley.size().y / 2.0),
            galley,
            colours.text,
        );
        if response.has_focus() {
            super::widgets::focus_ring(ui, rect);
        }
    }
    if response.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if response.has_focus() {
        ctx.data_mut(|data| data.insert_temp(focus_id(), response.id));
    }
    // Enter or Space on the focused value is a click: it opens the list,
    // or takes the highlighted choice while it is open.
    let keyboard = response.has_focus()
        && ui.input(|input| {
            input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Space)
        });
    if response.clicked() {
        if open && keyboard {
            if let Some((value, _)) = choices.get(memory.highlight) {
                changed = *current != *value;
                *current = *value;
            }
            open = false;
        } else {
            open = !open;
            memory.highlight = selected;
        }
        set_open(&ctx, open.then_some(id));
        response.request_focus();
    }

    // The keys, while the list is open and the value has the focus.
    if open && response.has_focus() {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    vertical_arrows: true,
                    escape: true,
                    ..Default::default()
                },
            );
        });
        let last = choices.len().saturating_sub(1);
        let (down, up, escape) = ui.input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            )
        });
        if down {
            memory.highlight = (memory.highlight + 1).min(last);
        }
        if up {
            memory.highlight = memory.highlight.saturating_sub(1);
        }
        if escape {
            open = false;
        }
    } else if !open
        && response.has_focus()
        && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
    {
        open = true;
        memory.highlight = selected;
    }

    // The choices, in place, sliding open.
    // Instant while the system asks for reduced motion.
    let reveal = ctx.animate_bool_with_time(
        id.with("reveal"),
        open,
        theme::motion_seconds(&ctx, SLIDE_SECONDS),
    );
    let mut list_rect = egui::Rect::NOTHING;
    if reveal > 0.0 {
        #[allow(clippy::cast_precision_loss, reason = "a short list")]
        let full = choices.len() as f32 * ROW_HEIGHT;
        let height = (full * reveal).round();
        let (area, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        list_rect = area;
        let painter = ui.painter().with_clip_rect(area.intersect(ui.clip_rect()));
        for (index, (value, label)) in choices.iter().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "a short list")]
            let row = egui::Rect::from_min_size(
                egui::pos2(area.left(), area.top() + index as f32 * ROW_HEIGHT),
                egui::vec2(width, ROW_HEIGHT),
            );
            if row.top() >= area.bottom() {
                break;
            }
            let row_response = ui.interact(
                row.intersect(area),
                id.with(("choice", index)),
                egui::Sense::click(),
            );
            let chosen = value == current;
            let highlighted = open && memory.highlight == index;
            if row_response.hovered() || highlighted {
                painter.rect_filled(row, 0.0, colours.hover);
            }
            if chosen {
                painter.rect_filled(
                    egui::Rect::from_min_size(row.min, egui::vec2(BAR_WIDTH, ROW_HEIGHT)),
                    0.0,
                    colours.accent,
                );
            }
            let ink = if chosen {
                colours.accent_text
            } else {
                colours.text
            };
            painter.text(
                egui::pos2(row.left() + INSET + BAR_WIDTH, row.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(VALUE_TEXT),
                ink,
            );
            if highlighted && response.has_focus() {
                painter.rect_stroke(
                    row.shrink(1.0),
                    0.0,
                    egui::Stroke::new(theme::FOCUS_RING_WIDTH, colours.accent),
                    egui::StrokeKind::Inside,
                );
            }
            if row_response.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if open && row_response.clicked() {
                changed = *current != *value;
                *current = *value;
                open = false;
                response.request_focus();
            }
        }
    }

    // The focus moved to another control (Tab, Shift+Tab, a click on a
    // field): the list closes. No focus at all (a click while the window
    // is not the key window, when egui holds no keyboard focus) keeps it
    // open: opening a list never depends on the keyboard. A press on the
    // list itself keeps it until its click lands.
    let focus_elsewhere = ctx
        .memory(egui::Memory::focused)
        .is_some_and(|focused| focused != response.id);
    let tabbed = ui.input(|input| input.key_pressed(egui::Key::Tab));
    let on_picker = ctx
        .pointer_interact_pos()
        .is_some_and(|pos| rect.contains(pos) || list_rect.contains(pos));
    let pressing = ui.input(|input| input.pointer.any_down() || input.pointer.any_released());
    if open && !response.has_focus() && (focus_elsewhere || tabbed) && !(pressing && on_picker) {
        open = false;
    }

    // A press anywhere else closes it.
    if open
        && ui.input(|input| input.pointer.any_pressed())
        && ctx
            .pointer_interact_pos()
            .is_some_and(|pos| !rect.contains(pos) && !list_rect.contains(pos))
    {
        open = false;
    }

    if open || is_open(&ctx, id) {
        // Refreshed each pass it is open, so a picker no longer drawn
        // stops counting as open.
        set_open(&ctx, open.then_some(id));
    }
    if !open && response.has_focus() {
        // Closed: Esc and the arrows are no longer the picker's.
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(response.id, egui::EventFilter::default());
        });
    }
    ui.data_mut(|data| data.insert_temp(id, memory));

    if let Some(help) = help.filter(|help| !help.is_empty()) {
        ui.add_space(theme::FIELD_TO_HELP);
        ui.scope(|ui| {
            ui.set_max_width(width);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(help)
                        .size(theme::TEXT_LABEL)
                        .color(colours.text2),
                )
                .wrap(),
            );
        });
    }
    PickerResponse {
        response,
        changed,
        open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fruit {
        Apple,
        Pear,
        Plum,
    }

    fn choices() -> Vec<(Fruit, String)> {
        vec![
            (Fruit::Apple, "Apple".to_owned()),
            (Fruit::Pear, "Pear".to_owned()),
            (Fruit::Plum, "Plum".to_owned()),
        ]
    }

    /// One pass; the picker's response and the shapes drawn.
    fn pass(
        ctx: &egui::Context,
        value: &mut Fruit,
        events: Vec<egui::Event>,
    ) -> (PickerResponse, Vec<egui::epaint::ClippedShape>) {
        let mut picked = None;
        let output = ctx.run_ui(
            egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 600.0),
                )),
                ..egui::RawInput::default()
            },
            |ui| {
                picked = Some(list_picker(
                    ui,
                    "fruit",
                    Some("Fruit"),
                    value,
                    &choices(),
                    Some("The one to eat."),
                    300.0,
                ));
                ui.label("Below");
            },
        );
        (picked.expect("drawn"), output.shapes)
    }

    fn click(at: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    fn key(key: egui::Key) -> Vec<egui::Event> {
        vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]
    }

    /// Where a text is drawn last (in the list, when it is open), and in
    /// which colour.
    fn text_at(
        shapes: &[egui::epaint::ClippedShape],
        text: &str,
    ) -> Option<(egui::Pos2, egui::Color32)> {
        shapes
            .iter()
            .rev()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(shape) if shape.galley.job.text == text => {
                    let colour = shape
                        .galley
                        .job
                        .sections
                        .first()
                        .map_or(shape.fallback_color, |section| section.format.color);
                    Some((shape.pos, colour))
                }
                _ => None,
            })
    }

    fn settle(
        ctx: &egui::Context,
        value: &mut Fruit,
    ) -> (PickerResponse, Vec<egui::epaint::ClippedShape>) {
        let mut last = pass(ctx, value, Vec::new());
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            last = pass(ctx, value, Vec::new());
        }
        last
    }

    #[test]
    fn a_click_opens_the_list_in_place_and_a_choice_closes_it() {
        let ctx = egui::Context::default();
        let mut fruit = Fruit::Apple;
        let (closed, shapes) = pass(&ctx, &mut fruit, Vec::new());
        assert!(!closed.open);
        let below_closed = text_at(&shapes, "Below").expect("below").0.y;
        assert!(text_at(&shapes, "Pear").is_none(), "closed: no choices");

        let _ = pass(&ctx, &mut fruit, click(closed.response.rect.center()));
        let (opened, shapes) = settle(&ctx, &mut fruit);
        assert!(opened.open);
        let below_open = text_at(&shapes, "Below").expect("below").0.y;
        assert!(
            below_open >= below_closed + 3.0 * ROW_HEIGHT - 1.0,
            "the list pushes what follows down: {below_closed} to {below_open}"
        );
        let (pear, _) = text_at(&shapes, "Pear").expect("Pear listed");
        let _ = pass(&ctx, &mut fruit, click(pear + egui::vec2(4.0, 4.0)));
        assert_eq!(fruit, Fruit::Pear);
        let (after, _) = pass(&ctx, &mut fruit, Vec::new());
        assert!(!after.open, "a choice closes the list");
    }

    #[test]
    fn the_keyboard_opens_moves_chooses_and_escape_closes() {
        let ctx = egui::Context::default();
        let mut fruit = Fruit::Apple;
        let (first, _) = pass(&ctx, &mut fruit, Vec::new());
        ctx.memory_mut(|memory| memory.request_focus(first.response.id));
        let _ = pass(&ctx, &mut fruit, Vec::new());
        let (opened, _) = pass(&ctx, &mut fruit, key(egui::Key::Enter));
        assert!(opened.open, "Enter opens");
        let _ = pass(&ctx, &mut fruit, key(egui::Key::ArrowDown));
        let _ = pass(&ctx, &mut fruit, key(egui::Key::ArrowDown));
        let (chosen, _) = pass(&ctx, &mut fruit, key(egui::Key::Enter));
        assert_eq!(fruit, Fruit::Plum);
        assert!(chosen.changed && !chosen.open);
        let _ = pass(&ctx, &mut fruit, key(egui::Key::Enter));
        let (escaped, _) = pass(&ctx, &mut fruit, key(egui::Key::Escape));
        assert!(!escaped.open, "Esc closes");
        assert_eq!(fruit, Fruit::Plum);
    }

    #[test]
    fn a_click_elsewhere_closes_it_and_the_chosen_one_is_in_the_accent() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            super::super::theme::apply(&ctx);
            ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            let palette = theme::Palette::of(dark);
            let mut fruit = Fruit::Pear;
            let (closed, _) = pass(&ctx, &mut fruit, Vec::new());
            let _ = pass(&ctx, &mut fruit, click(closed.response.rect.center()));
            let (_, shapes) = settle(&ctx, &mut fruit);
            let (_, colour) = text_at(&shapes, "Pear").expect("listed");
            assert_eq!(colour, palette.accent_text);
            for surface in [palette.field, palette.hover, palette.bg] {
                assert!(theme::contrast_ratio(palette.accent_text, surface) >= 4.5);
            }
            let (_, plain) = text_at(&shapes, "Plum").expect("listed");
            assert_eq!(plain, palette.text);
            let _ = pass(&ctx, &mut fruit, click(egui::pos2(580.0, 580.0)));
            let (after, _) = pass(&ctx, &mut fruit, Vec::new());
            assert!(!after.open, "a click elsewhere closes it");
            assert_eq!(fruit, Fruit::Pear);
        }
    }

    #[test]
    fn with_reduced_motion_the_list_opens_at_once() {
        let ctx = egui::Context::default();
        theme::set_reduce_motion(&ctx, true);
        let mut fruit = Fruit::Apple;
        let (closed, shapes) = pass(&ctx, &mut fruit, Vec::new());
        let below_closed = text_at(&shapes, "Below").expect("below").0.y;
        let _ = pass(&ctx, &mut fruit, click(closed.response.rect.center()));
        // The very next pass shows the whole list: no slide.
        let (_, shapes) = pass(&ctx, &mut fruit, Vec::new());
        let below_open = text_at(&shapes, "Below").expect("below").0.y;
        assert!(
            below_open >= below_closed + 3.0 * ROW_HEIGHT - 1.0,
            "{below_closed} to {below_open}"
        );
    }

    /// One pass of a button, the picker and a button.
    fn between(ctx: &egui::Context, value: &mut Fruit, events: Vec<egui::Event>) -> PickerResponse {
        let mut picked = None;
        let _ = ctx.run_ui(
            egui::RawInput {
                events,
                ..egui::RawInput::default()
            },
            |ui| {
                let _ = ui.button("Before");
                picked = Some(list_picker(
                    ui,
                    "fruit",
                    Some("Fruit"),
                    value,
                    &choices(),
                    None,
                    300.0,
                ));
                let _ = ui.button("After");
            },
        );
        picked.expect("drawn")
    }

    #[test]
    fn shift_tab_out_of_an_open_list_closes_it_and_esc_hands_the_keys_back() {
        let ctx = egui::Context::default();
        let mut fruit = Fruit::Apple;
        let first = between(&ctx, &mut fruit, Vec::new());
        ctx.memory_mut(|memory| memory.request_focus(first.response.id));
        let _ = between(&ctx, &mut fruit, Vec::new());
        assert!(between(&ctx, &mut fruit, key(egui::Key::Enter)).open);
        assert!(any_open(&ctx));
        let shift_tab = vec![egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        }];
        let _ = between(&ctx, &mut fruit, shift_tab);
        let after = between(&ctx, &mut fruit, Vec::new());
        assert!(
            !after.open && !any_open(&ctx),
            "the focus left: the list closes"
        );
        assert!(!after.response.has_focus());

        // Esc closes an open list, and the next Esc is no longer the
        // picker's: the focus leaves it as from any control.
        ctx.memory_mut(|memory| memory.request_focus(first.response.id));
        let _ = between(&ctx, &mut fruit, Vec::new());
        assert!(between(&ctx, &mut fruit, key(egui::Key::Enter)).open);
        let closed = between(&ctx, &mut fruit, key(egui::Key::Escape));
        assert!(!closed.open && !any_open(&ctx));
        let _ = between(&ctx, &mut fruit, key(egui::Key::Escape));
        let after = between(&ctx, &mut fruit, Vec::new());
        assert!(!after.response.has_focus(), "the second Esc reached egui");
    }

    #[test]
    fn a_list_no_longer_drawn_does_not_count_as_open() {
        let ctx = egui::Context::default();
        let mut fruit = Fruit::Apple;
        let (closed, _) = pass(&ctx, &mut fruit, Vec::new());
        let _ = pass(&ctx, &mut fruit, click(closed.response.rect.center()));
        assert!(any_open(&ctx));
        // Its page closed: the next passes draw something else.
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.label("Home");
            });
        }
        assert!(!any_open(&ctx));
    }

    /// One pass with the window focused or not (egui holds no keyboard
    /// focus while it is not).
    fn pass_focused(
        ctx: &egui::Context,
        value: &mut Fruit,
        focused: bool,
        events: Vec<egui::Event>,
    ) -> (PickerResponse, Vec<egui::epaint::ClippedShape>) {
        let mut picked = None;
        let output = ctx.run_ui(
            egui::RawInput {
                focused,
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 600.0),
                )),
                ..egui::RawInput::default()
            },
            |ui| {
                let _ = ui.button("Before");
                picked = Some(list_picker(
                    ui,
                    "fruit",
                    Some("Fruit"),
                    value,
                    &choices(),
                    None,
                    300.0,
                ));
                let _ = ui.button("After");
            },
        );
        (picked.expect("drawn"), output.shapes)
    }

    #[test]
    fn a_click_opens_the_list_and_a_choice_lands_with_or_without_keyboard_focus() {
        for focused in [false, true] {
            let ctx = egui::Context::default();
            theme::set_reduce_motion(&ctx, true);
            let mut fruit = Fruit::Apple;
            let (closed, _) = pass_focused(&ctx, &mut fruit, focused, Vec::new());
            let _ = pass_focused(
                &ctx,
                &mut fruit,
                focused,
                click(closed.response.rect.center()),
            );
            let (opened, shapes) = pass_focused(&ctx, &mut fruit, focused, Vec::new());
            assert!(opened.open, "focused {focused}: the click opened it");
            let (still, _) = pass_focused(&ctx, &mut fruit, focused, Vec::new());
            assert!(still.open, "focused {focused}: it stays open");
            let (pear, _) = text_at(&shapes, "Pear").expect("Pear listed");
            let _ = pass_focused(
                &ctx,
                &mut fruit,
                focused,
                click(pear + egui::vec2(4.0, 4.0)),
            );
            assert_eq!(fruit, Fruit::Pear, "focused {focused}: the choice landed");
            let (after, _) = pass_focused(&ctx, &mut fruit, focused, Vec::new());
            assert!(!after.open);
        }
    }

    #[test]
    fn tab_and_shift_tab_out_of_an_open_list_close_it() {
        for modifiers in [egui::Modifiers::NONE, egui::Modifiers::SHIFT] {
            let ctx = egui::Context::default();
            let mut fruit = Fruit::Apple;
            let first = between(&ctx, &mut fruit, Vec::new());
            ctx.memory_mut(|memory| memory.request_focus(first.response.id));
            let _ = between(&ctx, &mut fruit, Vec::new());
            assert!(between(&ctx, &mut fruit, key(egui::Key::Enter)).open);
            let tab = vec![egui::Event::Key {
                key: egui::Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }];
            let _ = between(&ctx, &mut fruit, tab);
            let after = between(&ctx, &mut fruit, Vec::new());
            assert!(!after.open && !any_open(&ctx), "{modifiers:?}: closed");
        }
    }
}
