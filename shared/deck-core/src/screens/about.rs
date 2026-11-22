// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! About Arcen Deck, for a Deck without a system About panel: the same
//! content as the macOS standard About panel (name, version, copyright and
//! source line), centred over the window.

/// What the user did in the About window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AboutAction {
    None,
    Close,
}

/// Draws the About window while `open`; `version` and `build` are the
/// Deck's own (`CFBundleShortVersionString (CFBundleVersion)` on a Mac).
#[must_use]
pub fn render_about(ctx: &egui::Context, version: &str, build: &str) -> AboutAction {
    let mut action = AboutAction::None;
    egui::Window::new("About Arcen Deck")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .fixed_size([300.0, 0.0])
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Arcen Deck")
                        .strong()
                        .size(15.0)
                        .color(super::theme::colours(ui).text),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("Version {version} ({build})"))
                        .size(11.0)
                        .color(super::theme::colours(ui).text2),
                );
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(crate::about::COPYRIGHT_LINE)
                        .size(11.0)
                        .color(super::theme::colours(ui).text2),
                );
                ui.add_space(6.0);
                ui.hyperlink_to("Source code", crate::about::SOURCE_URL);
                ui.add_space(10.0);
                if ui.button("OK").clicked() {
                    action = AboutAction::Close;
                }
            });
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_about_window_names_the_version_and_the_source() {
        let ctx = egui::Context::default();
        let mut text = String::new();
        // A window lays itself out on its first pass and shows on the next.
        for _ in 0..2 {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert_eq!(
                    render_about(ui.ctx(), "0.19.1", "0.19.1"),
                    AboutAction::None
                );
            });
            text = format!("{:?}", output.shapes);
        }
        assert!(text.contains("Version 0.19.1 (0.19.1)"));
        assert!(text.contains("TUTTIFRUTTI"));
        assert!(text.contains("Source code"));
    }
}
