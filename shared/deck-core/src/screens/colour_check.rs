// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Colour check panel: the checklist `arcen_media::colour_check` builds,
//! drawn the same on both Decks. The shell gathers the facts (display,
//! presentation, a decoded frame) and applies the actions.

use arcen_media::colour_check::{ColourCheckReport, ColourFix, Section, Status};
use arcen_media::test_pattern::TestPattern;

/// What the user asked for in the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourCheckAction {
    Apply(ColourFix),
    Copy,
    Save,
    Close,
}

/// What the panel reads and edits.
#[derive(Debug)]
pub struct ColourCheckView<'a> {
    /// The panel is shown (its close button returns `Close`).
    pub open: bool,
    /// The pattern the host is asked to show; the user may pick another.
    pub pattern: &'a mut TestPattern,
    /// The last action's result, if any.
    pub notice: Option<&'a str>,
}

fn status_colour(colours: &super::theme::Palette, status: Status) -> egui::Color32 {
    match status {
        Status::Pass => colours.success,
        Status::Info => colours.info,
        Status::Warn => colours.warning,
        Status::Fail => colours.error,
    }
}

/// Draws the panel as a window over whatever screen is showing.
#[must_use]
pub fn render_colour_check(
    ctx: &egui::Context,
    view: &mut ColourCheckView<'_>,
    report: &ColourCheckReport,
) -> Option<ColourCheckAction> {
    let mut action = None;
    let mut open = view.open;
    let screen = ctx.content_rect();
    egui::Window::new("Colour check")
        .id(egui::Id::new("arcen-colour-check"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .default_height((screen.height() - 80.0).clamp(320.0, 900.0))
        .default_pos(screen.right_top() + egui::vec2(-600.0, 40.0))
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(super::theme::colours_of(ctx).panel)
                .inner_margin(egui::Margin::symmetric(14, 12)),
        )
        .show(ctx, |ui| {
            ui.visuals_mut().override_text_color = Some(super::theme::colours(ui).text);
            let overall = report.overall();
            ui.label(
                egui::RichText::new(format!("{} {}", overall.symbol(), report.summary()))
                    .strong()
                    .size(14.0)
                    .color(status_colour(&super::theme::colours(ui), overall)),
            );
            if let Some(host) = &report.host_label {
                ui.label(egui::RichText::new(host).size(11.0));
            }
            ui.label(
                egui::RichText::new(
                    "Read-only on the host: the Deck never changes a host setting.",
                )
                .size(11.0)
                .color(super::theme::colours(ui).text2),
            );
            ui.horizontal_wrapped(|ui| {
                let recommended = report.recommended_fixes();
                if let Some(first) = recommended.first()
                    && ui
                        .button(egui::RichText::new(format!("Apply: {}", first.label())).strong())
                        .clicked()
                {
                    action = Some(ColourCheckAction::Apply(*first));
                }
                if ui.button("Copy report").clicked() {
                    action = Some(ColourCheckAction::Copy);
                }
                if ui.button("Save report").clicked() {
                    action = Some(ColourCheckAction::Save);
                }
            });
            if let Some(notice) = view.notice {
                ui.label(egui::RichText::new(notice).size(11.5).italics());
            }
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for section in Section::ALL {
                        let mut items = report.section(section).peekable();
                        if items.peek().is_none() {
                            continue;
                        }
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(section.title()).strong().size(13.5));
                        if section == Section::Exactness {
                            ui.horizontal(|ui| {
                                ui.label("Pattern:");
                                let mut selected = *view.pattern;
                                egui::ComboBox::from_id_salt("colour-check-pattern")
                                    .selected_text(selected.token())
                                    .show_ui(ui, |ui| {
                                        for pattern in TestPattern::ALL {
                                            ui.selectable_value(
                                                &mut selected,
                                                *pattern,
                                                format!(
                                                    "{} — {}",
                                                    pattern.token(),
                                                    pattern.exposes()
                                                ),
                                            );
                                        }
                                    });
                                *view.pattern = selected;
                            });
                        }
                        for item in items {
                            let status = item.status();
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(status.symbol())
                                        .strong()
                                        .size(14.0)
                                        .color(status_colour(&super::theme::colours(ui), status)),
                                );
                                ui.label(egui::RichText::new(item.title).strong().size(12.5));
                                ui.label(
                                    egui::RichText::new(item.finding.code())
                                        .monospace()
                                        .size(9.5)
                                        .color(super::theme::colours(ui).text2),
                                );
                            });
                            ui.indent(item.finding.code(), |ui| {
                                ui.label(egui::RichText::new(item.finding.meaning()).size(12.0));
                                for detail in &item.details {
                                    ui.label(
                                        egui::RichText::new(detail)
                                            .monospace()
                                            .size(10.5)
                                            .color(super::theme::colours(ui).text2),
                                    );
                                }
                                let advice = item.finding.action();
                                if !advice.is_empty() && status != Status::Pass {
                                    ui.label(
                                        egui::RichText::new(format!("What to do: {advice}"))
                                            .size(11.5)
                                            .color(status_colour(
                                                &super::theme::colours(ui),
                                                status,
                                            )),
                                    );
                                }
                                if !item.fixes.is_empty() {
                                    ui.horizontal_wrapped(|ui| {
                                        for fix in &item.fixes {
                                            let clicked = match fix {
                                                ColourFix::ReadDoc(link) => ui
                                                    .link(format!("📖 {}", link.title))
                                                    .on_hover_text(link.url())
                                                    .clicked(),
                                                other => ui.small_button(other.label()).clicked(),
                                            };
                                            if clicked {
                                                action = Some(ColourCheckAction::Apply(*fix));
                                            }
                                        }
                                    });
                                }
                            });
                            ui.add_space(4.0);
                        }
                    }
                });
        });
    if !open {
        action = Some(ColourCheckAction::Close);
    }
    action
}
