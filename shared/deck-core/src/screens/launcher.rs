// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The launcher's frame: the slim Arcen-orange bar above every screen that
//! is not a session or Settings, with the app's actions on its right (Help,
//! Settings). Both Decks draw it from here.

use super::theme;

/// An app action in the launcher bar. The bar holds app actions only, on
/// its right, in [`TopBarAction::ALL`] order; the left stays empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopBarAction {
    /// The user guide (Help ▸ About stays in the Help menu).
    Help,
    /// Settings.
    Settings,
}

impl TopBarAction {
    /// The bar's actions, left to right.
    pub const ALL: [Self; 2] = [Self::Help, Self::Settings];

    /// Its icon.
    #[must_use]
    pub const fn icon(self) -> super::icons::Icon {
        match self {
            Self::Help => super::icons::Icon::Help,
            Self::Settings => super::icons::Icon::Gear,
        }
    }

    /// Its tooltip and accessibility label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Help => "Help",
            Self::Settings => "Settings",
        }
    }
}

/// Where Help leads: the user guide, in the public source every release is
/// tagged in (the same guide "Learn more" opens at a section).
#[must_use]
pub fn user_guide_url() -> String {
    format!("{}/blob/main/docs/user-guide.md", crate::about::SOURCE_URL)
}

/// Opens the user guide in the browser (the Deck's egui shell hands the
/// URL to the system, as for every link).
pub fn open_user_guide(ctx: &egui::Context) {
    ctx.open_url(egui::OpenUrl::new_tab(user_guide_url()));
}

/// The bar's height, padding included.
pub const LAUNCHER_BAR_HEIGHT: f32 = 36.0;

/// The bar's fill: the Arcen accent of Connect and Add Connection. White on
/// it is 3.3:1, above WCAG's 3:1 for icons, so no darker shade is needed.
pub const LAUNCHER_BAR_FILL: egui::Color32 = theme::ACCENT;

/// The icons' colour.
pub const LAUNCHER_ICON: egui::Color32 = egui::Color32::WHITE;

/// How much white a hovered or focused icon's background gets over the
/// bar: a subtle lighter tint that keeps the white icon above 3:1.
pub const LAUNCHER_HOVER_WHITE_ALPHA: u8 = 16;

/// The icon buttons' side; the bar pads them by 4 points above and below.
const ICON_SIDE: f32 = 28.0;
const VERTICAL_PADDING: i8 = 4;
const HORIZONTAL_PADDING: i8 = 14;

/// Draws the launcher bar across the full width; returns the action
/// clicked.
pub fn render_launcher_top_bar(ui: &mut egui::Ui) -> Option<TopBarAction> {
    let mut clicked = None;
    bar(ui, |ui| {
        // Right to left: the last action is the rightmost.
        for action in TopBarAction::ALL.into_iter().rev() {
            if toolbar_icon(ui, action).clicked() {
                clicked = Some(action);
            }
        }
    });
    clicked
}

/// An action in the bar over Settings: Help, then Done where Home has the
/// gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsBarAction {
    /// The user guide.
    Help,
    /// Back to Home.
    Done,
}

/// The same bar over Settings, so it reads as the same app: Help, and
/// **Done** where the gear is on Home.
pub fn render_settings_top_bar(ui: &mut egui::Ui) -> Option<SettingsBarAction> {
    let mut clicked = None;
    bar(ui, |ui| {
        if done_control(ui).clicked() {
            clicked = Some(SettingsBarAction::Done);
        }
        if toolbar_icon(ui, TopBarAction::Help).clicked() {
            clicked = Some(SettingsBarAction::Help);
        }
    });
    clicked
}

/// Done on the bar: white text on a deeper orange (the primary's fill,
/// 4.5:1), darker under the pointer and pressed.
fn done_control(ui: &mut egui::Ui) -> egui::Response {
    let font = egui::FontId::proportional(theme::BUTTON_TEXT);
    let galley = ui
        .painter()
        .layout_no_wrap("Done".to_owned(), font, LAUNCHER_ICON);
    let size = egui::vec2(galley.size().x + 2.0 * 14.0, ICON_SIDE);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Done"));
    if ui.is_rect_visible(rect) {
        let fill = if response.is_pointer_button_down_on() {
            theme::darken(theme::PRIMARY_FILL, theme::PRIMARY_PRESSED_DARKEN_PERCENT)
        } else if response.hovered() || response.has_focus() {
            theme::darken(theme::PRIMARY_FILL, theme::PRIMARY_HOVER_DARKEN_PERCENT)
        } else {
            theme::PRIMARY_FILL
        };
        ui.painter().rect_filled(rect, 6.0, fill);
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, LAUNCHER_ICON);
        if response.has_focus() {
            ui.painter().rect_stroke(
                rect,
                7.0,
                egui::Stroke::new(theme::FOCUS_RING_WIDTH, LAUNCHER_ICON),
                egui::StrokeKind::Outside,
            );
        }
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// The bar itself: the accent, full width, `ICON_SIDE` tall plus its
/// padding, its controls laid out right to left from its right edge.
fn bar(ui: &mut egui::Ui, controls: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    egui::Frame::new()
        .fill(LAUNCHER_BAR_FILL)
        .inner_margin(egui::Margin::symmetric(
            HORIZONTAL_PADDING,
            VERTICAL_PADDING,
        ))
        .show(ui, |ui| {
            ui.set_width((width - 2.0 * f32::from(HORIZONTAL_PADDING)).max(0.0));
            ui.set_height(ICON_SIDE);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), controls);
        });
}

/// A white icon with no button background; hovering or focusing it lightens
/// the bar under it a little.
fn toolbar_icon(ui: &mut egui::Ui, action: TopBarAction) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ICON_SIDE, ICON_SIDE), egui::Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, action.label()));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if response.hovered() || response.has_focus() {
            painter.rect_filled(
                rect,
                6.0,
                egui::Color32::from_white_alpha(LAUNCHER_HOVER_WHITE_ALPHA),
            );
        }
        super::icons::paint(
            painter,
            action.icon(),
            rect.center(),
            super::icons::ICON_GRID,
            LAUNCHER_ICON,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.on_hover_text(action.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bar's height as drawn.
    fn drawn_height() -> (f32, Option<TopBarAction>) {
        let ctx = egui::Context::default();
        let mut height = 0.0;
        let mut action = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let top = ui.cursor().top();
            action = render_launcher_top_bar(ui);
            height = ui.min_rect().bottom() - top;
        });
        (height, action)
    }

    #[test]
    fn the_bar_is_slim() {
        let (height, action) = drawn_height();
        assert!(
            (height - LAUNCHER_BAR_HEIGHT).abs() < 0.5,
            "the bar is {height} points tall"
        );
        assert_eq!(action, None);
    }

    #[test]
    fn the_bar_holds_help_then_settings_on_the_right() {
        assert_eq!(
            TopBarAction::ALL,
            [TopBarAction::Help, TopBarAction::Settings]
        );
        // Drawn right-aligned in that order, nothing on the left.
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 600.0));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..egui::RawInput::default()
            },
            |ui| {
                let _ = render_launcher_top_bar(ui);
            },
        );
        // Where each painted icon sits: Help's ring is a circle, the gear's
        // outline a closed path.
        let mut help = None;
        let mut gear = None;
        for clipped in &output.shapes {
            match &clipped.shape {
                egui::Shape::Circle(circle) if circle.radius > 8.0 => help = Some(circle.center.x),
                egui::Shape::Path(path) if path.closed && path.points.len() == 32 => {
                    gear = Some(clipped.shape.visual_bounding_rect().center().x);
                }
                egui::Shape::Text(text) => {
                    panic!("the bar draws no text: {:?}", text.galley.job.text)
                }
                _ => {}
            }
        }
        let (help, gear) = (help.expect("Help drawn"), gear.expect("Settings drawn"));
        assert!(help < gear, "Help, then Settings");
        assert!(help > screen.center().x, "both on the right");
        assert!(gear > screen.right() - 60.0, "Settings at the right edge");
    }

    #[test]
    fn white_icons_keep_3_to_1_on_the_bar_and_under_hover() {
        let hovered = theme::over(
            LAUNCHER_BAR_FILL,
            egui::Color32::WHITE,
            LAUNCHER_HOVER_WHITE_ALPHA,
        );
        for background in [LAUNCHER_BAR_FILL, hovered] {
            let ratio = theme::contrast_ratio(LAUNCHER_ICON, background);
            assert!(ratio >= 3.0, "{ratio:.2}:1 on {background:?}");
        }
        let ratio = theme::contrast_ratio(LAUNCHER_ICON, LAUNCHER_BAR_FILL);
        assert!((3.3..3.4).contains(&ratio), "{ratio:.3}");
    }

    #[test]
    fn settings_has_the_same_bar_with_help_and_a_readable_done() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 600.0));
        let mut height = 0.0;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..egui::RawInput::default()
            },
            |ui| {
                let top = ui.cursor().top();
                assert_eq!(render_settings_top_bar(ui), None);
                height = ui.min_rect().bottom() - top;
            },
        );
        assert!((height - LAUNCHER_BAR_HEIGHT).abs() < 0.5, "{height}");
        let mut done = None;
        let mut bar_fill = false;
        for clipped in &output.shapes {
            match &clipped.shape {
                egui::Shape::Text(text) if text.galley.job.text == "Done" => {
                    done = Some(text.pos.x);
                }
                egui::Shape::Rect(rect) if rect.fill == LAUNCHER_BAR_FILL => bar_fill = true,
                _ => {}
            }
        }
        assert!(bar_fill, "the orange bar");
        assert!(
            done.expect("Done drawn") > screen.right() - 100.0,
            "Done at the right"
        );
        for fill in [
            theme::PRIMARY_FILL,
            theme::darken(theme::PRIMARY_FILL, theme::PRIMARY_HOVER_DARKEN_PERCENT),
            theme::darken(theme::PRIMARY_FILL, theme::PRIMARY_PRESSED_DARKEN_PERCENT),
        ] {
            assert!(theme::contrast_ratio(LAUNCHER_ICON, fill) >= 4.5);
        }
    }
}
