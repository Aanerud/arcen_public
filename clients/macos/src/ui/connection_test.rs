// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! **Test connection**: runs the connection test from the connection editor
//! and shows its report. The steps, findings and wording come from
//! `arcen_session::connection_diagnostics`; this file only runs it off the UI
//! thread and draws it.

use std::sync::{Arc, Mutex};

use arcen_session::connection_diagnostics::{DiagnosticReport, DiagnosticTarget, Verdict};

use crate::transport::diagnose::{deck_label, run, DiagnoseRequest};

/// A test in flight or finished.
pub struct ConnectionTest {
    /// What was typed, for the heading.
    pub target_label: String,
    /// Why the test could not start (an unreadable address).
    pub input_error: Option<String>,
    pending: Arc<Mutex<Option<DiagnosticReport>>>,
    report: Option<DiagnosticReport>,
    /// The report was copied to the clipboard.
    pub copied: bool,
}

/// What the user asked for in the report panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionTestAction {
    Copy,
    RunAgain,
    Close,
}

impl ConnectionTest {
    /// Starts testing `host` (which may carry `:port`) on `port`, or on the
    /// Pier default when `port` is empty. Repaints `ctx` when done.
    #[must_use]
    pub fn start(host: &str, port: &str, ctx: &egui::Context) -> Self {
        let port = port.trim();
        let parsed = match port {
            "" => DiagnosticTarget::parse(host),
            value => match value.parse::<u16>() {
                Ok(port) if port != 0 => DiagnosticTarget::parse_with_port(host, Some(port)),
                _ => Err(arcen_session::connection_diagnostics::TargetError::InvalidPort),
            },
        };
        let pending = Arc::new(Mutex::new(None));
        let target = match parsed {
            Ok(target) => target,
            Err(error) => {
                return Self {
                    target_label: host.trim().to_owned(),
                    input_error: Some(error.to_string()),
                    pending,
                    report: None,
                    copied: false,
                };
            }
        };
        let target_label = target.display();
        let request = DiagnoseRequest::with_remembered_pin(target);
        let slot = Arc::clone(&pending);
        let repaint = ctx.clone();
        let task = async move {
            let report = run(request).await;
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
            repaint.request_repaint();
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(task);
            }
            Err(_) => {
                std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        runtime.block_on(task);
                    }
                });
            }
        }
        Self {
            target_label,
            input_error: None,
            pending,
            report: None,
            copied: false,
        }
    }

    /// Picks up a finished report.
    pub fn poll(&mut self) {
        if self.report.is_none() {
            self.report = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        }
    }

    /// Whether the test is still running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.input_error.is_none() && self.report.is_none()
    }

    /// The finished report.
    #[must_use]
    pub const fn report(&self) -> Option<&DiagnosticReport> {
        self.report.as_ref()
    }

    /// The text copied to the clipboard.
    #[must_use]
    pub fn report_text(&self) -> Option<String> {
        self.report
            .as_ref()
            .map(|report| report.render_text(&deck_label()))
    }
}

fn verdict_colour(verdict: Verdict) -> egui::Color32 {
    match verdict {
        Verdict::Pass => egui::Color32::from_rgb(0x1E, 0x8E, 0x3E),
        Verdict::Info => egui::Color32::from_rgb(0x1A, 0x73, 0xE8),
        Verdict::Skipped => egui::Color32::from_rgb(0x9A, 0x9A, 0xA0),
        Verdict::Warning => egui::Color32::from_rgb(0xC2, 0x7C, 0x0E),
        Verdict::Fail => egui::Color32::from_rgb(0xC0, 0x20, 0x20),
    }
}

/// Draws the report panel.
pub fn render(ui: &mut egui::Ui, test: &ConnectionTest) -> Option<ConnectionTestAction> {
    let mut action = None;
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(0xF6, 0xF7, 0xFB))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgb(0xE0, 0xE2, 0xEA),
        ))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("Connection test — {}", test.target_label))
                    .strong()
                    .size(14.0)
                    .color(egui::Color32::BLACK),
            );
            ui.label(
                egui::RichText::new("No user name or password is sent.")
                    .size(11.0)
                    .color(egui::Color32::from_rgb(0x60, 0x60, 0x68)),
            );
            ui.horizontal(|ui| {
                if test.report().is_some() {
                    let label = if test.copied { "Copied" } else { "Copy report" };
                    if ui.button(label).clicked() {
                        action = Some(ConnectionTestAction::Copy);
                    }
                }
                if !test.running() && ui.button("Run again").clicked() {
                    action = Some(ConnectionTestAction::RunAgain);
                }
                if ui.button("Close").clicked() {
                    action = Some(ConnectionTestAction::Close);
                }
            });
            ui.add_space(8.0);
            if let Some(error) = &test.input_error {
                ui.label(
                    egui::RichText::new(error)
                        .color(verdict_colour(Verdict::Fail))
                        .size(13.0),
                );
            } else if let Some(report) = test.report() {
                ui.label(
                    egui::RichText::new(report.summary())
                        .strong()
                        .size(13.0)
                        .color(verdict_colour(report.overall())),
                );
                ui.add_space(8.0);
                for step in &report.steps {
                    let verdict = step.verdict();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(verdict.label())
                                .monospace()
                                .strong()
                                .size(11.0)
                                .color(verdict_colour(verdict)),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{}. {}",
                                step.step.number(),
                                step.step.title()
                            ))
                            .strong()
                            .size(12.5),
                        );
                    });
                    ui.label(egui::RichText::new(step.finding.meaning()).size(12.0));
                    for detail in &step.details {
                        ui.label(
                            egui::RichText::new(detail)
                                .monospace()
                                .size(10.5)
                                .color(egui::Color32::from_rgb(0x50, 0x50, 0x58)),
                        );
                    }
                    let advice = step.finding.action();
                    if !advice.is_empty() && verdict != Verdict::Pass {
                        ui.label(
                            egui::RichText::new(format!("What to do: {advice}"))
                                .size(11.5)
                                .color(verdict_colour(verdict)),
                        );
                    }
                    ui.label(
                        egui::RichText::new(step.finding.code())
                            .monospace()
                            .size(9.5)
                            .color(egui::Color32::from_rgb(0x9A, 0x9A, 0xA0)),
                    );
                    ui.add_space(6.0);
                }
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Testing name, UDP 18444, certificate, greeting, MTU and round trip…");
                });
            }
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_address_reports_without_running() {
        let ctx = egui::Context::default();
        let test = ConnectionTest::start("bad host", "", &ctx);
        assert!(!test.running());
        assert!(test.input_error.is_some());
        let test = ConnectionTest::start("pier.example.internal", "0", &ctx);
        assert_eq!(
            test.input_error.as_deref(),
            Some("the port must be a number from 1 to 65535")
        );
    }
}
