use egui::{Color32, Visuals};

pub const BG_PRIMARY: Color32 = Color32::from_rgb(0x0A, 0x0A, 0x10);
pub const BG_SECONDARY: Color32 = Color32::from_rgb(0x12, 0x12, 0x1C);
pub const BG_TERTIARY: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x28);
pub const BG_SURFACE: Color32 = Color32::from_rgb(0x22, 0x22, 0x34);
pub const BG_HOVER: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x40);

pub const ACCENT: Color32 = Color32::from_rgb(0xC9, 0x78, 0x50);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0xE2, 0xA1, 0x7E);
pub const GOLD: Color32 = Color32::from_rgb(0xD4, 0xA8, 0x55);

pub const DANGER: Color32 = Color32::from_rgb(0xE5, 0x48, 0x4D);
pub const WARNING: Color32 = Color32::from_rgb(0xF5, 0xA6, 0x23);
pub const SUCCESS: Color32 = Color32::from_rgb(0x46, 0xA7, 0x58);
pub const INFO: Color32 = Color32::from_rgb(0x3B, 0x9E, 0xFF);

pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xEC, 0xEC, 0xF1);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0x8B, 0x8B, 0xA3);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x4E, 0x4E, 0x6A);
pub const BORDER: Color32 = Color32::from_rgb(0x26, 0x26, 0x40);

pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::light();
    visuals.panel_fill = egui::Color32::WHITE;
    visuals.window_fill = egui::Color32::WHITE;
    visuals.extreme_bg_color = egui::Color32::WHITE;
    visuals.faint_bg_color = egui::Color32::from_rgb(0xF4, 0xF4, 0xF4);
    visuals.widgets.noninteractive.bg_fill = egui::Color32::WHITE;
    visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::DARK_GRAY;
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(0xF7, 0xF7, 0xF7);
    visuals.widgets.inactive.fg_stroke.color = egui::Color32::BLACK;
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0xF3, 0xEE, 0xE8);
    visuals.widgets.hovered.fg_stroke.color = egui::Color32::BLACK;
    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.active.fg_stroke.color = BG_PRIMARY;
    visuals.selection.bg_fill = ACCENT.linear_multiply(0.30);
    visuals.hyperlink_color = ACCENT;
    visuals.warn_fg_color = WARNING;
    visuals.error_fg_color = DANGER;
    ctx.set_visuals(visuals);
}
